use crate::{
    api::App,
    chat::{self, commands, committed_item},
    store::Lease,
};
use anyhow::{Context, ensure};
use ryzhub_execution::{GuestCommand, GuestEvent, GuestRequest, HostCommand, SandboxSpec};
use serde_json::{Value, json};
use uuid::Uuid;

pub async fn host(app: &App, command: HostCommand) -> anyhow::Result<Value> {
    ensure!(app.config.sandbox.enabled, "SANDBOX_DISABLED");
    // All RPCs carry durable operation identities. A disconnected response can safely
    // be retried with exactly the same request; application/fencing errors cannot.
    for attempt in 0..6 {
        match ryzhub_execution::call(
            std::path::Path::new(&app.config.sandbox.host_socket),
            &command,
        )
        .await
        {
            Ok(value) => return Ok(value),
            Err(error) => {
                let transient = error.downcast_ref::<std::io::Error>().is_some_and(|e| {
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound
                            | std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::TimedOut
                    )
                }) || error
                    .downcast_ref::<serde_json::Error>()
                    .is_some_and(|e| e.is_eof())
                    || error
                        .downcast_ref::<tokio::time::error::Elapsed>()
                        .is_some();
                if !transient || attempt == 5 {
                    return Err(error);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100 << attempt)).await;
            }
        }
    }
    unreachable!()
}
pub async fn ensure_sandbox(app: &App, lease: &Lease, run: Uuid) -> anyhow::Result<Uuid> {
    ensure!(app.config.sandbox.enabled, "SANDBOX_DISABLED");
    cleanup(app, lease).await?;
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::guard(&mut tx, lease, run).await?;
    let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND run_id<>$3 AND state IN ('running','provisioning'))").bind(lease.org_id).bind(lease.conversation_id).bind(run).fetch_one(&mut *tx).await?;
    ensure!(!busy, "WORKSPACE_BUSY");
    let existing:Option<Uuid>=sqlx::query_scalar("SELECT id FROM sandboxes WHERE org_id=$1 AND run_id=$2 AND state IN ('provisioning','running') ORDER BY created_at DESC LIMIT 1").bind(lease.org_id).bind(run).fetch_optional(&mut *tx).await?;
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
        sqlx::query("INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle) VALUES($1,$2,$3,$4,1,$5,'provisioning',$6)").bind(id).bind(lease.org_id).bind(lease.conversation_id).bind(run).bind(lease.epoch).bind(serde_json::to_value(&spec)?).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    host(app, HostCommand::Create { spec }).await?;
    guest(app, lease, id, GuestCommand::Fence).await?;
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::guard(&mut tx, lease, run).await?;
    sqlx::query("UPDATE sandboxes SET state='running',exec_epoch=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(id)
        .bind(lease.epoch)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    crate::projects::mount(app, lease, run, id).await?;
    if let Some(socket_path) = &app.config.llm.unix_socket {
        let mut tx = app.store.tx(lease.org_id).await?;
        let user: Uuid =
            sqlx::query_scalar("SELECT creator_id FROM conversations WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(lease.conversation_id)
                .fetch_one(&mut *tx)
                .await?;
        let credential: Option<Uuid> =
            sqlx::query_scalar("SELECT credential_id FROM runs WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(run)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        let mut claims = crate::llm::identity::loop_identity(lease, user, run, &app.config.llm);
        claims.kind = "sandbox".into();
        claims.sandbox_id = Some(id);
        claims.lease_epoch = Some(1);
        claims.credential_id = credential;
        let token = crate::llm::gateway::issue_sandbox_key(app, &mut claims).await?;
        let socket_name = std::path::Path::new(socket_path)
            .file_name()
            .context("INVALID_GATEWAY_SOCKET")?
            .to_str()
            .context("INVALID_GATEWAY_SOCKET")?
            .into();
        guest(
            app,
            lease,
            id,
            GuestCommand::RotateKey { token, socket_name },
        )
        .await?;
    }
    sync_events(app, lease, run, id).await?;
    Ok(id)
}
pub async fn guest(
    app: &App,
    lease: &Lease,
    id: Uuid,
    command: GuestCommand,
) -> anyhow::Result<Value> {
    host(
        app,
        HostCommand::Guest {
            request: GuestRequest {
                sandbox_id: id,
                exec_epoch: lease.epoch as u64,
                command,
            },
        },
    )
    .await
}
pub async fn sync_events(app: &App, lease: &Lease, run: Uuid, id: Uuid) -> anyhow::Result<()> {
    let mut tx = app.store.tx(lease.org_id).await?;
    let acked: i64 = sqlx::query_scalar(
        "SELECT acked_gseq FROM sandboxes WHERE org_id=$1 AND id=$2 AND run_id=$3",
    )
    .bind(lease.org_id)
    .bind(id)
    .bind(run)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let replay = guest(
        app,
        lease,
        id,
        GuestCommand::Replay {
            after: acked as u64,
        },
    )
    .await?;
    let events: Vec<GuestEvent> = serde_json::from_value(replay["events"].clone())?;
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::guard(&mut tx, lease, run).await?;
    let mut current: i64 =
        sqlx::query_scalar("SELECT acked_gseq FROM sandboxes WHERE org_id=$1 AND id=$2 FOR UPDATE")
            .bind(lease.org_id)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    for event in events {
        if event.gseq <= current as u64 {
            continue;
        }
        ensure!(event.gseq == current as u64 + 1, "GUEST_EVENT_GAP");
        sqlx::query("INSERT INTO guest_events(org_id,sandbox_id,gseq,payload) VALUES($1,$2,$3,$4) ON CONFLICT(sandbox_id,gseq) DO NOTHING").bind(lease.org_id).bind(id).bind(event.gseq as i64).bind(serde_json::to_value(&event)?).execute(&mut *tx).await?;
        let prefix = format!("{run}:");
        let call = event
            .op_id
            .strip_prefix(&prefix)
            .context("INVALID_GUEST_OPERATION")?;
        let result = if event.result["state"] == "outcome_unknown" {
            json!({"error":"outcome_unknown","details":event.result})
        } else {
            event.result
        };
        let updated:Option<Uuid>=sqlx::query_scalar("UPDATE tool_calls SET status='completed',result=$4,completed_at=now() WHERE org_id=$1 AND run_id=$2 AND call_id=$3 AND status IN ('started','outcome_unknown') RETURNING item_id").bind(lease.org_id).bind(run).bind(call).bind(&result).fetch_optional(&mut *tx).await?;
        if let Some(item) = updated {
            let mut item = commands::item(&mut tx, lease.org_id, item).await?;
            item.revision += 1;
            item.state = "completed".into();
            item.text = result.to_string();
            committed_item(&mut tx, lease, item).await?;
        }
        current = event.gseq as i64;
    }
    sqlx::query("UPDATE sandboxes SET acked_gseq=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(id)
        .bind(current)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    // This acknowledgement is intentionally after the PostgreSQL commit.
    guest(
        app,
        lease,
        id,
        GuestCommand::Ack {
            through: current as u64,
        },
    )
    .await?;
    Ok(())
}
pub async fn cleanup(app: &App, lease: &Lease) -> anyhow::Result<()> {
    let mut tx = app.store.tx(lease.org_id).await?;
    let ids:Vec<Uuid>=sqlx::query_scalar("SELECT s.id FROM sandboxes s JOIN runs r ON r.org_id=s.org_id AND r.id=s.run_id WHERE s.org_id=$1 AND s.conversation_id=$2 AND s.state IN ('running','provisioning') AND NOT s.persistent AND r.state IN ('completed','cancelled','failed')").bind(lease.org_id).bind(lease.conversation_id).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    for id in ids {
        host(
            app,
            HostCommand::Stop {
                sandbox_id: id,
                exec_epoch: lease.epoch as u64,
            },
        )
        .await?;
        let mut tx = app.store.tx(lease.org_id).await?;
        chat::lock(&mut tx, lease).await?;
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
