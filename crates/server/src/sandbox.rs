//! Execution-node access: host RPCs, ephemeral Chat sandboxes and node lease renewal.
use anyhow::{Context, ensure};
use ryzhub_execution::{GuestCommand, GuestEvent, GuestRequest, HostCommand, SandboxSpec};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

use crate::{app::App, error::Code, llm::keys, owner::OwnerLink, store::Lease};

/// How long a guest long poll may wait.
pub const LONG_POLL_MS: u64 = 10_000;

fn socket(app: &App) -> &Path {
    Path::new(&app.config.sandbox.host_socket)
}

/// A host RPC. Every request carries a durable operation identity, so a request whose reply
/// was lost to a broken connection is retried unchanged; application errors are not retried.
pub async fn host(app: &App, command: HostCommand) -> anyhow::Result<Value> {
    ensure!(app.config.sandbox.enabled, Code::SandboxDisabled);
    let mut attempt = 0;
    loop {
        match ryzhub_execution::call(socket(app), &command).await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < 5 && transient(&error) => {
                tokio::time::sleep(std::time::Duration::from_millis(100 << attempt)).await;
                attempt += 1;
            }
            Err(error) => return Err(peer_code(error)),
        }
    }
}

/// Peer rejections that mean something to callers carry their [`Code`]: a newer writer
/// (`FENCED`), a busy workspace, an unsupported request. Other peer errors stay internal; the
/// guest is not trusted to choose what clients see.
fn peer_code(error: anyhow::Error) -> anyhow::Error {
    let code = match error.downcast_ref::<ryzhub_execution::Rejected>() {
        Some(rejected) => match rejected.0.as_str() {
            "FENCED" => Code::Fenced,
            "WORKSPACE_BUSY" => Code::WorkspaceBusy,
            "PTY_UNSUPPORTED" => Code::PtyUnsupported,
            _ => return error,
        },
        None => return error,
    };
    error.context(code)
}

fn transient(error: &anyhow::Error) -> bool {
    use std::io::ErrorKind::*;
    error.downcast_ref::<std::io::Error>().is_some_and(|e| {
        matches!(
            e.kind(),
            NotFound | ConnectionRefused | ConnectionReset | BrokenPipe | UnexpectedEof | TimedOut
        )
    }) || error
        .downcast_ref::<serde_json::Error>()
        .is_some_and(|e| e.is_eof())
        || error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
}

/// Poll an asynchronous host job (checkpoint, restore) until it finishes.
pub async fn host_job(app: &App, command: HostCommand) -> anyhow::Result<Value> {
    loop {
        let reply = host(app, command.clone()).await?;
        match reply["state"].as_str() {
            Some("running") => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
            Some("failed") => anyhow::bail!(
                "execution node job failed: {}",
                reply["error"].as_str().unwrap_or("unknown")
            ),
            _ => return Ok(reply),
        }
    }
}

/// A command for the guest agent of `sandbox`, fenced by the lease's epoch.
pub async fn guest(
    app: &App,
    lease: &Lease,
    sandbox: Uuid,
    command: GuestCommand,
) -> anyhow::Result<Value> {
    host(
        app,
        HostCommand::Guest {
            request: GuestRequest {
                sandbox_id: sandbox,
                exec_epoch: lease.epoch as u64,
                command,
            },
        },
    )
    .await
}

/// The run's ephemeral Chat sandbox, created on first use. Only `/workspace` persists.
pub async fn ensure_sandbox(
    app: &App,
    lease: &Lease,
    run: Uuid,
    link: &OwnerLink,
) -> anyhow::Result<Uuid> {
    ensure!(app.config.sandbox.enabled, Code::SandboxDisabled);
    cleanup(app, lease).await?;
    let (id, spec) = {
        let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
        let busy: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND run_id<>$3
             AND state IN ('running','provisioning'))",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(!busy, Code::WorkspaceBusy);
        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM sandboxes WHERE org_id=$1 AND run_id=$2 AND state IN ('provisioning','running')
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(lease.org_id)
        .bind(run)
        .fetch_optional(&mut *tx)
        .await?;
        let id = existing.unwrap_or_else(Uuid::now_v7);
        let config = &app.config.sandbox;
        let spec = SandboxSpec {
            id,
            org_id: lease.org_id,
            workspace_id: lease.conversation_id,
            exec_epoch: lease.epoch as u64,
            image: config.image.clone(),
            runtime: config.runtime.clone(),
            memory_bytes: config.memory_bytes,
            pids_limit: config.pids_limit,
            persistent: false,
        };
        if existing.is_none() {
            sqlx::query(
                "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle)
                 VALUES($1,$2,$3,$4,1,$5,'provisioning',$6)",
            )
            .bind(id)
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .bind(run)
            .bind(lease.epoch)
            .bind(serde_json::to_value(&spec)?)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        (id, spec)
    };
    host(app, HostCommand::Create { spec }).await?;
    guest(app, lease, id, GuestCommand::Fence).await?;
    {
        let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
        sqlx::query("UPDATE sandboxes SET state='running',exec_epoch=$3 WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(id)
            .bind(lease.epoch)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    crate::projects::mount(app, lease, run, id).await?;
    crate::attachments::sandbox::sync(app, lease, id).await?;
    // A fresh key per dispatch supersedes the previous one; accepted streams continue.
    if app.config.llm.unix_socket.is_some() {
        let (user, credential) = run_identity(app, lease, run).await?;
        let key = keys::issue_sandbox_key(
            &app.store,
            keys::SandboxKeyRequest {
                org_id: lease.org_id,
                user_id: user,
                conversation_id: lease.conversation_id,
                run_id: run,
                sandbox_id: id,
                lease_epoch: 1,
                models: vec![app.config.llm.model.clone()],
                credential_id: credential,
            },
        )
        .await?;
        rotate_key(app, lease, id, key.token).await?;
    }
    sync_events(app, lease, run, id, link).await?;
    Ok(id)
}

/// The conversation creator and the run's selected credential.
pub async fn run_identity(
    app: &App,
    lease: &Lease,
    run: Uuid,
) -> anyhow::Result<(Uuid, Option<Uuid>)> {
    let mut tx = app.store.tx(lease.org_id).await?;
    let row: (Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT c.creator_id,r.credential_id FROM runs r
         JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id
         WHERE r.org_id=$1 AND r.id=$2",
    )
    .bind(lease.org_id)
    .bind(run)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// Hand the guest a new sandbox key; it lives only in guest memory.
pub async fn rotate_key(
    app: &App,
    lease: &Lease,
    sandbox: Uuid,
    token: String,
) -> anyhow::Result<()> {
    let socket_name = Path::new(
        app.config
            .llm
            .unix_socket
            .as_ref()
            .context("Work requires llm.unix_socket")?,
    )
    .file_name()
    .context("invalid gateway socket")?
    .to_string_lossy()
    .into_owned();
    guest(
        app,
        lease,
        sandbox,
        GuestCommand::RotateKey { token, socket_name },
    )
    .await?;
    Ok(())
}

/// Record completed Chat tool operations reported by the guest, then acknowledge them.
/// The acknowledgement is sent only after the database commit.
pub async fn sync_events(
    app: &App,
    lease: &Lease,
    run: Uuid,
    sandbox: Uuid,
    link: &OwnerLink,
) -> anyhow::Result<()> {
    let acked: i64 = {
        let mut tx = app.store.tx(lease.org_id).await?;
        let acked = sqlx::query_scalar(
            "SELECT acked_gseq FROM sandboxes WHERE org_id=$1 AND id=$2 AND run_id=$3",
        )
        .bind(lease.org_id)
        .bind(sandbox)
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        acked
    };
    let replay = guest(
        app,
        lease,
        sandbox,
        GuestCommand::Replay {
            after: acked as u64,
            wait_ms: 0,
            folds_seen: 0,
        },
    )
    .await?;
    let events: Vec<GuestEvent> = serde_json::from_value(replay["events"].clone())?;
    let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
    let mut current: i64 =
        sqlx::query_scalar("SELECT acked_gseq FROM sandboxes WHERE org_id=$1 AND id=$2 FOR UPDATE")
            .bind(lease.org_id)
            .bind(sandbox)
            .fetch_one(&mut *tx)
            .await?;
    let prefix = format!("{run}:");
    let acked = current as u64;
    for event in events.into_iter().filter(|e| e.gseq > acked) {
        ensure!(event.gseq == current as u64 + 1, "guest event gap");
        sqlx::query(
            "INSERT INTO guest_events(org_id,sandbox_id,gseq,payload) VALUES($1,$2,$3,$4)
             ON CONFLICT(sandbox_id,gseq) DO NOTHING",
        )
        .bind(lease.org_id)
        .bind(sandbox)
        .bind(event.gseq as i64)
        .bind(serde_json::to_value(&event)?)
        .execute(&mut *tx)
        .await?;
        let call = event
            .op_id
            .strip_prefix(&prefix)
            .context("guest operation outside this run")?;
        let result = if event.result["state"] == "outcome_unknown" {
            json!({"error":"outcome_unknown","details":event.result})
        } else {
            event.result
        };
        let item: Option<Uuid> = sqlx::query_scalar(
            "UPDATE tool_calls SET status='completed',result=$4,completed_at=now()
             WHERE org_id=$1 AND run_id=$2 AND call_id=$3 AND status IN ('started','outcome_unknown')
             RETURNING item_id",
        )
        .bind(lease.org_id)
        .bind(run)
        .bind(call)
        .bind(&result)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(item) = item {
            crate::chat::tools::complete_item(&mut tx, lease, item, &result).await?;
        }
        current = event.gseq as i64;
    }
    sqlx::query("UPDATE sandboxes SET acked_gseq=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(sandbox)
        .bind(current)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    link.committed().await;
    guest(
        app,
        lease,
        sandbox,
        GuestCommand::Ack {
            through: current as u64,
        },
    )
    .await?;
    Ok(())
}

/// Stop ephemeral Chat sandboxes whose run has ended.
pub async fn cleanup(app: &App, lease: &Lease) -> anyhow::Result<()> {
    let ended: Vec<Uuid> = {
        let mut tx = app.store.tx(lease.org_id).await?;
        let ids = sqlx::query_scalar(
            "SELECT s.id FROM sandboxes s JOIN runs r ON r.org_id=s.org_id AND r.id=s.run_id
             WHERE s.org_id=$1 AND s.conversation_id=$2 AND s.state IN ('running','provisioning')
               AND NOT s.persistent AND r.state IN ('completed','cancelled','failed')",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        ids
    };
    for id in ended {
        host(
            app,
            HostCommand::Stop {
                sandbox_id: id,
                exec_epoch: lease.epoch as u64,
            },
        )
        .await?;
        let mut tx = app.store.fenced(lease).await?;
        sqlx::query("UPDATE sandboxes SET state='stopped',exec_epoch=$3 WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(id)
            .bind(lease.epoch)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}

/// Renew node leases of persistent sandboxes the execution node reports alive. A sandbox whose
/// lease expires is treated as lost and restored from its last checkpoint. Runs in both the
/// Owner and gateway roles so a rolling restart of either keeps leases alive.
pub async fn heartbeat(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    loop {
        if let Ok(status) = host(&app, HostCommand::Status).await {
            let alive: Vec<Uuid> = serde_json::from_value(status["running"].clone())?;
            let persistent: Vec<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT org_id,id FROM sandboxes WHERE persistent AND state='running'",
            )
            .fetch_all(&worker)
            .await?;
            for (org, id) in persistent.into_iter().filter(|(_, id)| alive.contains(id)) {
                let mut tx = app.store.tx(org).await?;
                sqlx::query(
                    "UPDATE sandboxes SET node_lease_until=now()+interval '15 seconds'
                     WHERE org_id=$1 AND id=$2 AND node_lease_until>clock_timestamp()",
                )
                .bind(org)
                .bind(id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::code_of;

    #[test]
    fn only_allowlisted_peer_rejections_carry_codes() {
        let rejected = |text: &str| anyhow::Error::from(ryzhub_execution::Rejected(text.into()));
        assert_eq!(code_of(&peer_code(rejected("FENCED"))), Some(Code::Fenced));
        assert_eq!(
            code_of(&peer_code(rejected("PTY_UNSUPPORTED"))),
            Some(Code::PtyUnsupported)
        );
        // A guest cannot make clients see an arbitrary code.
        assert_eq!(
            code_of(&peer_code(rejected("CHECKPOINT_RECOVERY_RPO"))),
            None
        );
        assert_eq!(code_of(&peer_code(anyhow::anyhow!("FENCED"))), None);
    }
}
