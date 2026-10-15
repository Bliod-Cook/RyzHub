//! Execution-node access: host RPCs, ephemeral Chat sandboxes and node lease renewal.
//!
//! Every sandbox runs on one execution node, recorded when it is placed. RPCs about a sandbox
//! go to its node; commands about a whole workspace go to the nodes its sandboxes ran on.
use anyhow::{Context, ensure};
use ryzhub_execution::{
    Endpoint, GuestCommand, GuestEvent, GuestRequest, HostCommand, SandboxSpec,
};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, sync::Mutex};
use uuid::Uuid;

use crate::{
    app::App,
    error::Code,
    llm::keys,
    owner::OwnerLink,
    store::{Lease, Tx},
};

/// How long a guest long poll may wait.
pub const LONG_POLL_MS: u64 = 10_000;

/// TLS endpoints by node name, built once: loading keys and certificates is not free.
static TLS_ENDPOINTS: Mutex<Option<HashMap<String, Endpoint>>> = Mutex::new(None);

/// Where `node`'s host agent listens: its Unix socket, or its TLS listener.
pub fn node_endpoint(app: &App, node: &str) -> anyhow::Result<Endpoint> {
    let config = app
        .config
        .sandbox
        .nodes()
        .into_iter()
        .find(|n| n.name == node)
        .with_context(|| format!("execution node {node:?} is not configured"))?;
    if config.address.is_empty() {
        return Ok(Endpoint::Unix(config.host_socket.into()));
    }
    let mut cache = TLS_ENDPOINTS.lock().unwrap();
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(endpoint) = cache.get(node) {
        return Ok(endpoint.clone());
    }
    let endpoint = Endpoint::tls(
        &config.address,
        &config.server_name,
        Path::new(&config.tls_ca),
        Path::new(&config.tls_cert),
        Path::new(&config.tls_key),
    )?;
    cache.insert(node.to_owned(), endpoint.clone());
    Ok(endpoint)
}

/// The node `sandbox` runs on. With one node configured that is the answer without a lookup.
pub async fn node_of(app: &App, org: Uuid, sandbox: Uuid) -> anyhow::Result<String> {
    if let [only] = app.config.sandbox.nodes().as_slice() {
        return Ok(only.name.clone());
    }
    let mut tx = app.store.tx(org).await?;
    let node: Option<String> =
        sqlx::query_scalar("SELECT node FROM sandboxes WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(sandbox)
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    node.context("UNKNOWN_SANDBOX")
}

/// The node a new sandbox of conversation `conv` runs on, registered within `tx`; the caller
/// records it on the sandbox's row. Of the nodes that are active, not known to be down and
/// below capacity, it is the one the conversation last ran on (its workspace is there), else
/// the least loaded.
pub async fn place(tx: &mut Tx<'_>, app: &App, conv: Uuid) -> anyhow::Result<String> {
    let nodes = app.config.sandbox.nodes();
    let chosen = if let [only] = nodes.as_slice() {
        only.clone()
    } else {
        let load: HashMap<String, i64> = sqlx::query_as("SELECT node,live FROM node_load()")
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .collect();
        let down: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM execution_nodes
             WHERE state<>'active' OR lease_until<clock_timestamp()",
        )
        .fetch_all(&mut **tx)
        .await?;
        let last: Option<String> = sqlx::query_scalar(
            "SELECT node FROM sandboxes WHERE conversation_id=$1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(conv)
        .fetch_optional(&mut **tx)
        .await?;
        let live = |name: &str| load.get(name).copied().unwrap_or(0);
        let open: Vec<_> = nodes
            .into_iter()
            .filter(|n| !down.contains(&n.name))
            .filter(|n| n.capacity == 0 || live(&n.name) < i64::from(n.capacity))
            .collect();
        match open.iter().find(|n| last.as_ref() == Some(&n.name)) {
            Some(node) => node.clone(),
            None => open
                .into_iter()
                .min_by_key(|n| live(&n.name))
                .context(Code::NoExecutionNode)?,
        }
    };
    sqlx::query(
        "INSERT INTO execution_nodes(name,capacity) VALUES($1,$2)
         ON CONFLICT(name) DO UPDATE SET capacity=excluded.capacity,updated_at=now()
         WHERE execution_nodes.capacity<>excluded.capacity",
    )
    .bind(&chosen.name)
    .bind(chosen.capacity as i32)
    .execute(&mut **tx)
    .await?;
    Ok(chosen.name)
}

/// What a new sandbox of `org` may use: egress rules (the instance allowlist narrowed by
/// policy) and the size of its writable layer.
pub struct Limits {
    pub egress: Vec<String>,
    pub disk_bytes: u64,
}

pub async fn limits(tx: &mut Tx<'_>, app: &App, org: Uuid) -> anyhow::Result<Limits> {
    let config = &app.config.sandbox;
    let policy = crate::store::effective_policy(tx, &app.config.policy, org).await?;
    let egress = config
        .egress_allow
        .iter()
        .filter(|rule| {
            policy
                .egress_hosts
                .as_ref()
                .is_none_or(|hosts| hosts.contains(*rule))
        })
        .cloned()
        .collect();
    // The node refuses limits below 256 MiB.
    let disk_bytes = policy
        .sandbox_disk_bytes
        .map_or(config.disk_bytes, |cap| cap.min(config.disk_bytes))
        .max(256 * 1024 * 1024);
    Ok(Limits { egress, disk_bytes })
}

/// A new sandbox of the lease's conversation: the configured image, layer and resources,
/// within `limits`. Work sandboxes (`persistent`) get at least 512 MiB of memory.
pub fn spec(app: &App, lease: &Lease, id: Uuid, persistent: bool, limits: Limits) -> SandboxSpec {
    let config = &app.config.sandbox;
    SandboxSpec {
        id,
        org_id: lease.org_id,
        workspace_id: lease.conversation_id,
        exec_epoch: lease.epoch as u64,
        image: config.image.clone(),
        runtime: config.runtime.clone(),
        memory_bytes: if persistent {
            config.memory_bytes.max(512 << 20)
        } else {
            config.memory_bytes
        },
        pids_limit: config.pids_limit,
        persistent,
        egress: limits.egress,
        disk_bytes: limits.disk_bytes,
        deps: (!config.deps.is_empty()).then(|| config.deps.clone()),
    }
}

/// Where to stream artifacts and files for `sandbox`: its node.
pub async fn endpoint(app: &App, org: Uuid, sandbox: Uuid) -> anyhow::Result<Endpoint> {
    node_endpoint(app, &node_of(app, org, sandbox).await?)
}

/// A host RPC to `node`. Every request carries a durable operation identity, so a request
/// whose reply was lost to a broken connection is retried unchanged; application errors are
/// not retried.
pub async fn host_on(app: &App, node: &str, command: HostCommand) -> anyhow::Result<Value> {
    ensure!(app.config.sandbox.enabled, Code::SandboxDisabled);
    let endpoint = node_endpoint(app, node)?;
    let mut attempt = 0;
    loop {
        match ryzhub_execution::call(&endpoint, &command).await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < 5 && transient(&error) => {
                tokio::time::sleep(std::time::Duration::from_millis(100 << attempt)).await;
                attempt += 1;
            }
            Err(error) => return Err(peer_code(error)),
        }
    }
}

/// A host RPC about `sandbox`, sent to its node.
pub async fn host(
    app: &App,
    org: Uuid,
    sandbox: Uuid,
    command: HostCommand,
) -> anyhow::Result<Value> {
    host_on(app, &node_of(app, org, sandbox).await?, command).await
}

/// A host RPC about workspace `workspace` as a whole, sent to each node one of its sandboxes
/// ran on: those hold its files. A node no longer configured is gone along with its files.
pub async fn host_workspace(
    app: &App,
    org: Uuid,
    workspace: Uuid,
    command: HostCommand,
) -> anyhow::Result<()> {
    let configured = app.config.sandbox.nodes();
    let nodes: Vec<String> = if let [only] = configured.as_slice() {
        vec![only.name.clone()]
    } else {
        let mut tx = app.store.tx(org).await?;
        let used: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT node FROM sandboxes WHERE org_id=$1 AND conversation_id=$2",
        )
        .bind(org)
        .bind(workspace)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        used.into_iter()
            .filter(|node| configured.iter().any(|n| &n.name == node))
            .collect()
    };
    for node in nodes {
        host_on(app, &node, command.clone()).await?;
    }
    Ok(())
}

/// Peer rejections that mean something to callers carry their [`Code`]: a newer writer
/// (`FENCED`), a busy workspace. Other peer errors stay internal; the guest is not trusted to
/// choose what clients see.
fn peer_code(error: anyhow::Error) -> anyhow::Error {
    let code = match error.downcast_ref::<ryzhub_execution::Rejected>() {
        Some(rejected) => match rejected.0.as_str() {
            "FENCED" => Code::Fenced,
            "WORKSPACE_BUSY" => Code::WorkspaceBusy,
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

/// Poll an asynchronous host job (checkpoint, restore) about `sandbox` until it finishes.
pub async fn host_job(
    app: &App,
    org: Uuid,
    sandbox: Uuid,
    command: HostCommand,
) -> anyhow::Result<Value> {
    let node = node_of(app, org, sandbox).await?;
    loop {
        let reply = host_on(app, &node, command.clone()).await?;
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
        lease.org_id,
        sandbox,
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
    cleanup(app, lease, true).await?;
    let (id, spec, fresh) = {
        let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
        // This run's sandbox, or one an earlier run of the conversation left idle.
        let live: Option<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT id,run_id FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND NOT persistent
             AND state IN ('provisioning','running') ORDER BY created_at DESC LIMIT 1 FOR UPDATE",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((id, owner)) = live
            && owner != run
        {
            let ended: bool = sqlx::query_scalar(
                "SELECT state IN ('completed','cancelled','failed') FROM runs WHERE org_id=$1 AND id=$2",
            )
            .bind(lease.org_id)
            .bind(owner)
            .fetch_one(&mut *tx)
            .await?;
            ensure!(ended, Code::WorkspaceBusy);
            sqlx::query("UPDATE sandboxes SET run_id=$3,idle_until=NULL WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(id)
                .bind(run)
                .execute(&mut *tx)
                .await?;
        }
        let existing = live.map(|(id, _)| id);
        let id = existing.unwrap_or_else(Uuid::now_v7);
        let spec = spec(
            app,
            lease,
            id,
            false,
            limits(&mut tx, app, lease.org_id).await?,
        );
        if existing.is_none() {
            // Its workspace is restored onto the node below if the node has no copy.
            sqlx::query(
                "UPDATE workspace_heads SET evicted_at=NULL WHERE org_id=$1 AND conversation_id=$2",
            )
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .execute(&mut *tx)
            .await?;
            let node = place(&mut tx, app, lease.conversation_id).await?;
            sqlx::query(
                "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,node)
                 VALUES($1,$2,$3,$4,1,$5,'provisioning',$6,$7)",
            )
            .bind(id)
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .bind(run)
            .bind(lease.epoch)
            .bind(serde_json::to_value(&spec)?)
            .bind(node)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        (id, spec, existing.is_none())
    };
    // A new sandbox may land on a node without the workspace: bring back its checkpoint.
    if fresh && chat_checkpoints(app) {
        crate::workspace::restore_workspace(app, lease, id, spec.disk_bytes).await?;
    }
    host(app, lease.org_id, id, HostCommand::Create { spec }).await?;
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
        let user = run_creator(app, lease, run).await?;
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
            },
        )
        .await?;
        rotate_key(app, lease, id, key.token).await?;
    }
    sync_events(app, lease, run, id, link).await?;
    Ok(id)
}

/// The creator of the run's conversation.
pub async fn run_creator(app: &App, lease: &Lease, run: Uuid) -> anyhow::Result<Uuid> {
    let mut tx = app.store.tx(lease.org_id).await?;
    let user = sqlx::query_scalar(
        "SELECT c.creator_id FROM runs r
         JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id
         WHERE r.org_id=$1 AND r.id=$2",
    )
    .bind(lease.org_id)
    .bind(run)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(user)
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
        // A sandbox serves the conversation's runs one after another; an earlier run's
        // operations are stored but complete nothing of this run.
        let Some(call) = event.op_id.strip_prefix(&prefix) else {
            current = event.gseq as i64;
            continue;
        };
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

/// Whether Chat workspaces are checkpointed: configured, with storage to keep them in.
pub fn chat_checkpoints(app: &App) -> bool {
    app.config.sandbox.chat_checkpoints && !app.config.work.storage_root.is_empty()
}

/// Whether the conversation's workspace may have changed since its last checkpoint: a run
/// used a sandbox and ended (or is ending) after it.
async fn workspace_changed(app: &App, lease: &Lease) -> anyhow::Result<bool> {
    let mut tx = app.store.tx(lease.org_id).await?;
    let changed = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tool_calls t JOIN runs r ON r.org_id=t.org_id AND r.id=t.run_id
         WHERE t.org_id=$1 AND r.conversation_id=$2 AND t.sandbox_id IS NOT NULL
           AND coalesce(r.ended_at,now())>coalesce((SELECT max(c.created_at) FROM checkpoints c
               WHERE c.org_id=$1 AND c.conversation_id=$2 AND c.state='committed'),'-infinity'))",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(changed)
}

/// Checkpoint the workspace on `sandbox`'s node if it changed. A failure only delays it: the
/// node keeps the workspace, and the next run or expiry tries again.
async fn checkpoint_if_changed(app: &App, lease: &Lease, sandbox: Uuid) {
    if !chat_checkpoints(app) {
        return;
    }
    let outcome = async {
        if workspace_changed(app, lease).await? {
            crate::workspace::checkpoint_workspace(app, lease, sandbox).await?;
        }
        anyhow::Ok(())
    }
    .await;
    if let Err(error) = outcome {
        tracing::warn!(conv = %lease.conversation_id, error = format!("{error:#}"), "workspace checkpoint deferred");
    }
}

/// Chat sandboxes after their run: each stays up `sandbox.chat_idle_secs` for the next run to
/// reuse, with the workspace checkpointed once the run ends, then is destroyed. Destroy
/// unmounts the sandbox's overlay and image and keeps the separate `/workspace` directory.
///
/// A run about to use the sandbox (`reusing`) does not wait for the previous run's checkpoint:
/// its own end checkpoints both runs' changes.
pub async fn cleanup(app: &App, lease: &Lease, reusing: bool) -> anyhow::Result<()> {
    let (ended, expired): (Vec<Uuid>, Vec<Uuid>) = {
        let mut tx = app.store.fenced(lease).await?;
        let ended = sqlx::query_scalar(
            "UPDATE sandboxes s SET idle_until=now()+($3::bigint*interval '1 second') FROM runs r
             WHERE r.org_id=s.org_id AND r.id=s.run_id AND s.org_id=$1 AND s.conversation_id=$2
               AND NOT s.persistent AND s.state IN ('running','provisioning') AND s.idle_until IS NULL
               AND r.state IN ('completed','cancelled','failed')
             RETURNING s.id",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .bind(app.config.sandbox.chat_idle_secs as i64)
        .fetch_all(&mut *tx)
        .await?;
        let expired = sqlx::query_scalar(
            "SELECT id FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND NOT persistent
             AND state IN ('running','provisioning') AND idle_until<=now()",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        (ended, expired)
    };
    for id in &ended {
        if !reusing && !expired.contains(id) {
            checkpoint_if_changed(app, lease, *id).await;
        }
    }
    for id in expired {
        checkpoint_if_changed(app, lease, id).await;
        host(
            app,
            lease.org_id,
            id,
            HostCommand::Destroy {
                sandbox_id: id,
                exec_epoch: lease.epoch as u64,
            },
        )
        .await?;
        let mut tx = app.store.fenced(lease).await?;
        sqlx::query("UPDATE sandboxes SET state='deleted',exec_epoch=$3 WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(id)
            .bind(lease.epoch)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}

/// How long a running Work workspace may go without a committed checkpoint while in use
/// before it is reported: the recovery point objective is 15 minutes.
pub const CHECKPOINT_STALE_SECS: i64 = 20 * 60;

/// Periodic node housekeeping in the Owner role: destroy stopped Work sandboxes, move unused
/// checkpointed Chat workspaces off their nodes, and report workspaces whose checkpoints fall
/// behind.
pub async fn reclaim(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    loop {
        if let Err(error) = reclaim_once(&app, &worker).await {
            tracing::warn!(error = format!("{error:#}"), "node reclamation deferred");
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

pub async fn reclaim_once(app: &App, worker: &sqlx::PgPool) -> anyhow::Result<()> {
    let config = &app.config.sandbox;
    // A stopped sandbox is never used again: resuming, restoring and recovering a Work
    // workspace all start a new one from a checkpoint, and a Chat run only reuses a running
    // one. With no writer left to fence out, it is destroyed under the last epoch: fences the
    // node took after its row last changed (later owners') do not hold it up, and the node's
    // tombstone refuses a late Create.
    let stopped: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT org_id,id FROM sandboxes WHERE state IN ('suspended','archived','lost')
         AND coalesce(node_lease_until,created_at)<now()-($1::bigint*interval '1 second') LIMIT 100",
    )
    .bind(config.reclaim_after_secs as i64)
    .fetch_all(worker)
    .await?;
    for (org, id) in stopped {
        let destroy = HostCommand::Destroy {
            sandbox_id: id,
            exec_epoch: u64::MAX,
        };
        if let Err(error) = host(app, org, id, destroy).await {
            tracing::warn!(sandbox = %id, error = format!("{error:#}"), "stopped sandbox not reclaimed");
            continue;
        }
        let mut tx = app.store.tx(org).await?;
        sqlx::query(
            "UPDATE sandboxes SET state='deleted' WHERE org_id=$1 AND id=$2
             AND state IN ('suspended','archived','lost')",
        )
        .bind(org)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    if chat_checkpoints(app) && config.workspace_evict_after_secs > 0 {
        evict_workspaces(app, worker).await?;
    }
    report_stale_checkpoints(app, worker).await?;
    // Images change rarely: an hourly look is enough.
    static IMAGES_AT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let due = {
        let mut at = IMAGES_AT.lock().unwrap();
        let due = at.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(3600));
        if due {
            *at = Some(std::time::Instant::now());
        }
        due
    };
    if due {
        collect_images(app, worker).await?;
    }
    Ok(())
}

/// Images this control plane references now (configuration, a sandbox not yet deleted, a
/// checkpoint that may still be restored), and every image it has ever used. Only the latter,
/// less the former, may be removed: images another control plane or an operator put on the
/// node are never this one's to remove.
async fn image_references(
    app: &App,
    worker: &sqlx::PgPool,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let config = &app.config.sandbox;
    let configured = [&config.image, &config.deps]
        .into_iter()
        .filter(|image| !image.is_empty())
        .cloned();
    let query = |live: bool| {
        let filter = if live { "WHERE state<>'deleted'" } else { "" };
        format!(
            "SELECT DISTINCT image FROM (
               SELECT handle->>'image' AS image FROM sandboxes {filter}
               UNION SELECT handle->>'deps' FROM sandboxes {filter}
               UNION SELECT manifest->>'image' FROM checkpoints {filter}
               UNION SELECT manifest->>'deps' FROM checkpoints {filter}) images
             WHERE image IS NOT NULL"
        )
    };
    let mut current: Vec<String> = configured.collect();
    current.extend(
        sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(query(true)))
            .fetch_all(worker)
            .await?,
    );
    let used = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(query(false)))
        .fetch_all(worker)
        .await?;
    Ok((current, used))
}

/// Remove digest-pinned images this control plane used and no longer references, once that
/// has lasted `image_gc_after_secs`, from their nodes. The node refuses to remove one a
/// sandbox there uses.
pub async fn collect_images(app: &App, worker: &sqlx::PgPool) -> anyhow::Result<()> {
    let (current, used) = image_references(app, worker).await?;
    let grace = app.config.sandbox.image_gc_after_secs as i64;
    for node in app.config.sandbox.nodes() {
        let Ok(listing) = host_on(app, &node.name, HostCommand::Images).await else {
            continue;
        };
        let held: Vec<(String, bool)> = listing["images"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| Some((i["reference"].as_str()?.to_owned(), i["in_use"] == true)))
            .collect();
        let mut tx = app.store.pool.begin().await?;
        sqlx::query("INSERT INTO execution_nodes(name) VALUES($1) ON CONFLICT DO NOTHING")
            .bind(&node.name)
            .execute(&mut *tx)
            .await?;
        // Forget images that are gone or referenced again.
        let unreferenced: Vec<&str> = held
            .iter()
            .filter(|(image, in_use)| !in_use && used.contains(image) && !current.contains(image))
            .map(|(image, _)| image.as_str())
            .collect();
        sqlx::query("DELETE FROM node_images WHERE node=$1 AND NOT reference=ANY($2)")
            .bind(&node.name)
            .bind(&unreferenced)
            .execute(&mut *tx)
            .await?;
        let expired: Vec<String> = sqlx::query_scalar::<_, Option<String>>(
            "INSERT INTO node_images(node,reference) SELECT $1,unnest($2::text[])
             ON CONFLICT(node,reference) DO UPDATE SET unreferenced_since=node_images.unreferenced_since
             RETURNING CASE WHEN unreferenced_since<now()-($3::bigint*interval '1 second') THEN reference END",
        )
        .bind(&node.name)
        .bind(&unreferenced)
        .bind(grace)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .flatten()
        .collect();
        tx.commit().await?;
        for image in expired {
            let remove = HostCommand::RemoveImage {
                reference: image.clone(),
            };
            match host_on(app, &node.name, remove).await {
                Ok(_) => {
                    sqlx::query("DELETE FROM node_images WHERE node=$1 AND reference=$2")
                        .bind(&node.name)
                        .bind(&image)
                        .execute(&app.store.pool)
                        .await?;
                    tracing::info!(node = %node.name, image, "unused image removed");
                }
                Err(error) => {
                    tracing::warn!(node = %node.name, image, error = format!("{error:#}"), "image not removed")
                }
            }
        }
    }
    Ok(())
}

/// Move checkpointed Chat workspaces unused for `workspace_evict_after_secs` off their nodes,
/// when their head checkpoint holds every change. The conversation's lease row is locked
/// meanwhile, so no run places a sandbox between the check and the removal: a run that comes
/// first keeps the workspace, one that comes after restores it.
async fn evict_workspaces(app: &App, worker: &sqlx::PgPool) -> anyhow::Result<()> {
    let idle = app.config.sandbox.workspace_evict_after_secs as i64;
    let candidates: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT h.org_id,h.conversation_id FROM workspace_heads h
         JOIN checkpoints c ON c.org_id=h.org_id AND c.id=h.checkpoint_id
         WHERE h.evicted_at IS NULL AND c.state='committed' AND c.manifest->>'scope'='workspace'
           AND NOT EXISTS(SELECT 1 FROM runs r WHERE r.org_id=h.org_id AND r.conversation_id=h.conversation_id
                          AND coalesce(r.ended_at,'infinity')>now()-($1::bigint*interval '1 second'))
           AND NOT EXISTS(SELECT 1 FROM tool_calls t JOIN runs r ON r.org_id=t.org_id AND r.id=t.run_id
                          WHERE r.org_id=h.org_id AND r.conversation_id=h.conversation_id
                            AND t.sandbox_id IS NOT NULL AND coalesce(r.ended_at,'infinity')>c.created_at)
         LIMIT 100",
    )
    .bind(idle)
    .fetch_all(worker)
    .await?;
    for (org, conv) in candidates {
        let mut tx = app.store.tx(org).await?;
        sqlx::query("SELECT id FROM channels WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE")
            .bind(org)
            .bind(conv)
            .execute(&mut *tx)
            .await?;
        let live: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sandboxes WHERE org_id=$1 AND conversation_id=$2
             AND state IN ('running','provisioning'))",
        )
        .bind(org)
        .bind(conv)
        .fetch_one(&mut *tx)
        .await?;
        if live {
            continue;
        }
        let evict = HostCommand::EvictWorkspace {
            org_id: org,
            workspace_id: conv,
        };
        if let Err(error) = host_workspace(app, org, conv, evict).await {
            tracing::warn!(conv = %conv, error = format!("{error:#}"), "workspace not evicted");
            continue;
        }
        sqlx::query(
            "UPDATE workspace_heads SET evicted_at=now() WHERE org_id=$1 AND conversation_id=$2",
        )
        .bind(org)
        .bind(conv)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

/// Warn about, and audit at most hourly, running Work workspaces in use whose last committed
/// checkpoint is older than [`CHECKPOINT_STALE_SECS`].
async fn report_stale_checkpoints(app: &App, worker: &sqlx::PgPool) -> anyhow::Result<()> {
    let stale: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(concat!(
        "SELECT s.org_id,s.conversation_id,s.id FROM sandboxes s WHERE ",
        stale_checkpoint!()
    ))
    .bind(CHECKPOINT_STALE_SECS)
    .fetch_all(worker)
    .await?;
    for (org, conv, sandbox) in stale {
        tracing::warn!(conv = %conv, sandbox = %sandbox, "workspace checkpoint is stale");
        let mut tx = app.store.tx(org).await?;
        let reported: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM audit_logs WHERE org_id=$1 AND action='workspace.checkpoint_stale'
             AND resource=$2 AND created_at>now()-interval '1 hour')",
        )
        .bind(org)
        .bind(conv.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if !reported {
            crate::store::audit_as(
                &mut tx,
                org,
                sandbox,
                "sandbox",
                "workspace.checkpoint_stale",
                &conv.to_string(),
                "warning",
            )
            .await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

/// Store `node`'s new egress log records as audit records of their organizations. The node's
/// row is locked throughout, so the roles that both run the heartbeat store each record once.
/// Records of sandboxes this database does not know (another control plane sharing the node)
/// are passed over.
pub async fn collect_egress(app: &App, node: &str) -> anyhow::Result<usize> {
    let mut tx = app.store.pool.begin().await?;
    let (boot, after): (Option<Uuid>, i64) = sqlx::query_as(
        "SELECT egress_boot,egress_seq FROM execution_nodes WHERE name=$1 FOR UPDATE",
    )
    .bind(node)
    .fetch_one(&mut *tx)
    .await?;
    let reply = host_on(
        app,
        node,
        HostCommand::Egress {
            boot,
            after: after as u64,
        },
    )
    .await?;
    let current: Uuid = serde_json::from_value(reply["boot"].clone())?;
    let records: Vec<ryzhub_execution::egress::Record> =
        serde_json::from_value(reply["records"].clone())?;
    let mut last = if boot == Some(current) { after } else { 0 };
    let mut stored = 0;
    for record in &records {
        sqlx::query("SELECT set_config('app.org_id',$1,true)")
            .bind(record.org_id.to_string())
            .execute(&mut *tx)
            .await?;
        stored += sqlx::query(
            "INSERT INTO audit_logs(id,org_id,actor_id,actor_kind,action,resource,result,detail,created_at)
             SELECT $1,$2,$3,'sandbox','egress.connect',$4,$5,$6,to_timestamp($7::float8/1000)
             WHERE EXISTS(SELECT 1 FROM sandboxes WHERE org_id=$2 AND id=$3)",
        )
        .bind(Uuid::now_v7())
        .bind(record.org_id)
        .bind(record.sandbox_id)
        .bind(format!("{}:{}", record.host, record.port))
        .bind(if record.allowed { "allowed" } else { "denied" })
        .bind(json!({"reason": record.reason, "sent": record.sent, "received": record.received}))
        .bind(record.at_ms as f64)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        last = last.max(record.seq as i64);
    }
    sqlx::query("UPDATE execution_nodes SET egress_boot=$2,egress_seq=$3 WHERE name=$1")
        .bind(node)
        .bind(current)
        .bind(last)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(stored as usize)
}

/// Renew node leases of persistent sandboxes their execution node reports alive, and the
/// node's own lease. A sandbox whose lease expires is treated as lost and restored from its
/// last checkpoint. Runs in both the Owner and gateway roles so a rolling restart of either
/// keeps leases alive.
pub async fn heartbeat(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    loop {
        for node in &app.config.sandbox.nodes() {
            let Ok(status) = host_on(&app, &node.name, HostCommand::Status).await else {
                continue;
            };
            let alive: Vec<Uuid> = serde_json::from_value(status["running"].clone())?;
            let persistent: Vec<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT org_id,id FROM sandboxes WHERE persistent AND state='running' AND node=$1",
            )
            .bind(&node.name)
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
            sqlx::query(
                "INSERT INTO execution_nodes(name,capacity,lease_until) VALUES($1,$2,now()+interval '15 seconds')
                 ON CONFLICT(name) DO UPDATE SET lease_until=excluded.lease_until,
                   capacity=excluded.capacity,updated_at=now()",
            )
            .bind(&node.name)
            .bind(node.capacity as i32)
            .execute(&app.store.pool)
            .await?;
            if let Err(error) = collect_egress(&app, &node.name).await {
                tracing::warn!(node = %node.name, error = format!("{error:#}"), "egress log not collected");
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
            code_of(&peer_code(rejected("WORKSPACE_BUSY"))),
            Some(Code::WorkspaceBusy)
        );
        // A guest cannot make clients see an arbitrary code.
        assert_eq!(
            code_of(&peer_code(rejected("CHECKPOINT_RECOVERY_RPO"))),
            None
        );
        assert_eq!(code_of(&peer_code(anyhow::anyhow!("FENCED"))), None);
    }
}
