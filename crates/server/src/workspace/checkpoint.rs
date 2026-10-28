//! Creating a checkpoint.
use anyhow::Context;
use ryzhub_execution::{GuestCommand, HostCommand};
use serde_json::{Value, json};
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
/// 3. The checkpoint is published: registered, stored, then committed as the head.
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
        lease.org_id,
        sandbox_id,
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
    let settings = work::run_settings(app, lease, run).await?;
    manifest["thread_id"] = json!(thread);
    manifest["harness"] = json!(settings.harness);
    manifest["harness_version"] = json!(crate::run_config::harness_version(
        &app.config(),
        &settings.harness
    ));
    publish(app, lease, sandbox_id, artifact, manifest, Some(gseq)).await?;
    Ok(artifact)
}

/// Checkpoint the Chat workspace on `sandbox`'s node into a new checkpoint and make it the
/// head. Taken between runs, when nothing writes to it, so its watermark is the channel head;
/// nothing on the timeline is tied to it.
pub async fn workspace(app: &App, lease: &Lease, sandbox: Uuid) -> anyhow::Result<Uuid> {
    let artifact = Uuid::now_v7();
    let manifest = sandbox::host_job(
        app,
        lease.org_id,
        sandbox,
        HostCommand::WorkspaceCheckpoint {
            org_id: lease.org_id,
            workspace_id: lease.conversation_id,
            artifact,
        },
    )
    .await?;
    publish(app, lease, sandbox, artifact, manifest, None).await?;
    Ok(artifact)
}

/// Register the archive the node built for `sandbox` as checkpoint `artifact`, `uploading` so
/// garbage collection keeps the object, stream it to every store, then commit it as the head.
/// Its timeline watermark is where the guest event `gseq` was recorded; without one, the
/// channel head.
async fn publish(
    app: &App,
    lease: &Lease,
    sandbox: Uuid,
    artifact: Uuid,
    mut manifest: Value,
    gseq: Option<u64>,
) -> anyhow::Result<()> {
    let key = manifest["sha256"]
        .as_str()
        .context("checkpoint without checksum")?
        .to_owned();
    manifest
        .as_object_mut()
        .context("manifest")?
        .remove("state");
    {
        let mut tx = app.store.tx(lease.org_id).await?;
        blob_lock(&mut tx, lease.org_id, &key).await?;
        lock_lease(&mut tx, lease).await?;
        let recorded: Option<i64> = match gseq {
            Some(gseq) => sqlx::query_scalar(
                "SELECT channel_seq FROM guest_events WHERE org_id=$1 AND sandbox_id=$2 AND gseq=$3",
            )
            .bind(lease.org_id)
            .bind(sandbox)
            .bind(gseq as i64)
            .fetch_optional(&mut *tx)
            .await?
            .flatten(),
            None => None,
        };
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
        .bind(sandbox)
        .bind(&manifest)
        .bind(channel_seq)
        .bind(gseq.unwrap_or(0) as i64)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    let stored = async {
        let node = &sandbox::endpoint(app, lease.org_id, sandbox).await?;
        let download = &HostCommand::ArtifactDownload { artifact };
        let archive = || ryzhub_execution::download(node, download);
        stores(app, lease.org_id)?.put(&key, archive).await
    };
    if let Err(error) = stored.await {
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
    let _ = sandbox::host(
        app,
        lease.org_id,
        sandbox,
        HostCommand::DeleteArtifact { artifact },
    )
    .await;
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
         ON CONFLICT(org_id,conversation_id) DO UPDATE SET checkpoint_id=$3,evicted_at=NULL,
           storage_state=CASE WHEN workspace_heads.storage_state IN ('archiving','restoring')
                              THEN workspace_heads.storage_state ELSE 'hot' END",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(artifact)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
