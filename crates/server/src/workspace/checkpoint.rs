//! Creating a checkpoint.
use anyhow::Context;
use ryzhub_execution::{GuestCommand, HostCommand, native::harness_version};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

use super::{blob_lock, stores};
use crate::{
    app::App,
    error::Code,
    owner::OwnerLink,
    sandbox,
    store::{Lease, head_seq, lock_lease},
    work,
};

/// A checkpoint of the workspace as it is now: a new one (into `artifact`) from the running
/// sandbox, or the head a suspended workspace was stopped with, since nothing changed after it.
pub async fn current(
    app: &App,
    lease: &Lease,
    artifact: Uuid,
    link: &OwnerLink,
) -> anyhow::Result<Uuid> {
    if running_sandbox(app, lease).await?.is_some() {
        return create(app, lease, artifact, link).await;
    }
    suspended_head(app, lease)
        .await?
        .context(Code::NoCheckpoint)
}

/// The conversation's running persistent sandbox.
pub async fn running_sandbox(app: &App, lease: &Lease) -> anyhow::Result<Option<Uuid>> {
    let mut tx = app.store.fenced(lease).await?;
    let id = sqlx::query_scalar(
        "SELECT id FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent AND state='running'",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// The committed head checkpoint of a workspace whose latest sandbox is suspended.
pub async fn suspended_head(app: &App, lease: &Lease) -> anyhow::Result<Option<Uuid>> {
    let mut tx = app.store.fenced(lease).await?;
    let head = sqlx::query_scalar(
        "SELECT h.checkpoint_id FROM workspace_heads h
         JOIN checkpoints c ON c.org_id=h.org_id AND c.id=h.checkpoint_id AND c.state='committed'
         WHERE h.org_id=$1 AND h.conversation_id=$2
           AND (SELECT s.state FROM sandboxes s WHERE s.org_id=h.org_id AND s.conversation_id=h.conversation_id
                AND s.persistent ORDER BY s.created_at DESC LIMIT 1)='suspended'",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    tx.commit().await?;
    Ok(head)
}

/// Checkpoint the conversation's running sandbox into `artifact` and make it the head.
///
/// 1. The node freezes the sandbox only to copy its upper layer, then builds the archive.
/// 2. Guest events up to the freeze point are projected, fixing the timeline watermark.
/// 3. The checkpoint is registered as `uploading` (so garbage collection keeps the object),
///    the archive is streamed to every store, and only then committed as the head.
///
/// No database transaction is open while bytes move.
pub async fn create(
    app: &App,
    lease: &Lease,
    artifact: Uuid,
    link: &OwnerLink,
) -> anyhow::Result<Uuid> {
    let (sandbox_id, run, thread): (Uuid, Uuid, Option<String>) = {
        let mut tx = app.store.fenced(lease).await?;
        let row = sqlx::query_as(
            "SELECT id,run_id,native_thread_id FROM sandboxes WHERE org_id=$1 AND conversation_id=$2
             AND persistent AND state='running'",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .fetch_optional(&mut *tx)
        .await?
        .context("no running workspace to checkpoint")?;
        tx.commit().await?;
        row
    };
    sandbox::guest(app, lease, sandbox_id, GuestCommand::Fence).await?;
    let mut manifest = sandbox::host_job(
        app,
        HostCommand::Checkpoint {
            sandbox_id,
            exec_epoch: lease.epoch as u64,
            artifact,
        },
    )
    .await?;
    let gseq = manifest["gseq"]
        .as_u64()
        .context("checkpoint without watermark")?;
    work::sync_through(app, lease, run, sandbox_id, gseq, link).await?;
    let key = manifest["sha256"]
        .as_str()
        .context("checkpoint without checksum")?
        .to_owned();
    let size = manifest["size"].as_u64().unwrap_or(0);
    let settings = work::run_settings(app, lease, run).await?;
    manifest["thread_id"] = json!(thread);
    manifest["harness"] = json!(settings.harness);
    manifest["harness_version"] = json!(harness_version(&settings.harness));
    manifest["secondary"] = json!(app.config.work.secondary_root.is_some());
    manifest
        .as_object_mut()
        .context("manifest")?
        .remove("state");
    {
        let mut tx = app.store.tx(lease.org_id).await?;
        blob_lock(&mut tx, lease.org_id, &key).await?;
        lock_lease(&mut tx, lease).await?;
        let recorded: Option<i64> = sqlx::query_scalar(
            "SELECT channel_seq FROM guest_events WHERE org_id=$1 AND sandbox_id=$2 AND gseq=$3",
        )
        .bind(lease.org_id)
        .bind(sandbox_id)
        .bind(gseq as i64)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        // No harness event yet means nothing on the timeline depends on this sandbox.
        let channel_seq = match recorded {
            Some(seq) => seq,
            None => head_seq(&mut tx, lease.org_id, &lease.channel()).await? as i64,
        };
        sqlx::query(
            "INSERT INTO checkpoints(id,org_id,conversation_id,sandbox_id,manifest,channel_seq,gseq,state)
             VALUES($1,$2,$3,$4,$5,$6,$7,'uploading') ON CONFLICT DO NOTHING",
        )
        .bind(artifact)
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .bind(sandbox_id)
        .bind(&manifest)
        .bind(channel_seq)
        .bind(gseq as i64)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    if let Err(error) = upload(app, lease, artifact, &key, size).await {
        // Leave nothing half-published: the collector deletes the object unless shared.
        let mut tx = app.store.tx(lease.org_id).await?;
        sqlx::query(
            "UPDATE checkpoints SET state='tombstoned',delete_after=now() WHERE org_id=$1 AND id=$2 AND state='uploading'",
        )
        .bind(lease.org_id)
        .bind(artifact)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Err(error);
    }
    commit(app, lease, artifact, &manifest).await?;
    // The node's copy is no longer needed once stored.
    let _ = sandbox::host(app, HostCommand::DeleteArtifact { artifact }).await;
    Ok(artifact)
}

/// Stream the node's artifact into the primary and (if configured) secondary store.
async fn upload(
    app: &App,
    lease: &Lease,
    artifact: Uuid,
    key: &str,
    size: u64,
) -> anyhow::Result<()> {
    let (primary, secondary) = stores(app, lease.org_id)?;
    let socket = Path::new(&app.config.sandbox.host_socket);
    for store in std::iter::once(primary).chain(secondary) {
        let (length, reader) =
            ryzhub_execution::download(socket, &HostCommand::ArtifactDownload { artifact }).await?;
        anyhow::ensure!(size == 0 || length == size, "artifact size changed");
        store.put_stream(key, reader, length).await?;
    }
    Ok(())
}

async fn commit(app: &App, lease: &Lease, artifact: Uuid, manifest: &Value) -> anyhow::Result<()> {
    let mut tx = app.store.fenced(lease).await?;
    sqlx::query("UPDATE checkpoints SET state='committed',manifest=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(artifact)
        .bind(manifest)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO workspace_heads(org_id,conversation_id,checkpoint_id) VALUES($1,$2,$3)
         ON CONFLICT(org_id,conversation_id) DO UPDATE SET checkpoint_id=$3",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(artifact)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
