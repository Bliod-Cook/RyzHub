//! Restoring a checkpoint.
use anyhow::{Context, ensure};
use ryzhub_execution::{HostCommand, SandboxSpec};
use ryzhub_proto::{Item, event_envelope::Event};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

use super::stores;
use crate::{
    app::App,
    error::Code,
    owner::OwnerLink,
    runs, sandbox,
    store::{Lease, append, load_item, upsert_item},
};

/// A checkpoint can be restored only by the harness version that wrote its native session.
fn compatible(config: &crate::config::Config, manifest: &Value) -> bool {
    let harness = manifest["harness"].as_str().unwrap_or("codex");
    let version = manifest["harness_version"]
        .as_str()
        .or_else(|| manifest["codex_version"].as_str());
    manifest["image"]
        .as_str()
        .is_some_and(|image| image.contains("@sha256:"))
        && manifest["format"] == 1
        && manifest["encoding"] == "tar.zst"
        && version == Some(crate::run_config::harness_version(config, harness).as_str())
}

/// Make the primary store's copy valid (repairing it from the secondary if needed) and
/// stream it to execution node `node` as `artifact`.
async fn upload(app: &App, org: Uuid, key: &str, artifact: Uuid, node: &str) -> anyhow::Result<()> {
    let (primary, secondary) = stores(app, org)?;
    if !primary.verify(key).await? {
        let secondary = secondary.context("checkpoint missing and no secondary store")?;
        primary.repair_from(&secondary, key).await?;
    }
    let (size, reader) = primary.open(key).await?;
    ryzhub_execution::upload(
        &sandbox::node_endpoint(app, node)?,
        &HostCommand::ArtifactUpload {
            artifact,
            size,
            sha256: key.into(),
        },
        size,
        reader,
    )
    .await?;
    Ok(())
}

/// Bring the Chat workspace's head checkpoint back on `sandbox`'s node if that node has no copy
/// of the workspace (it ran elsewhere, or the node lost it). The node keeps a copy it has: it
/// is at least as new. Returns whether the checkpoint was restored.
pub async fn workspace_into(
    app: &App,
    lease: &Lease,
    sandbox: Uuid,
    disk_bytes: u64,
) -> anyhow::Result<bool> {
    let (org, conv) = (lease.org_id, lease.conversation_id);
    let head: Option<Value> = {
        let mut tx = app.store.tx(org).await?;
        let head = sqlx::query_scalar(
            "SELECT c.manifest FROM workspace_heads w
             JOIN checkpoints c ON c.org_id=w.org_id AND c.id=w.checkpoint_id
             WHERE w.org_id=$1 AND w.conversation_id=$2 AND c.state='committed'
               AND c.manifest->>'scope'='workspace'",
        )
        .bind(org)
        .bind(conv)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        head
    };
    let Some(manifest) = head else {
        return Ok(false);
    };
    let node = sandbox::node_of(app, org, sandbox).await?;
    let status = sandbox::host_on(
        app,
        &node,
        HostCommand::WorkspaceStatus {
            org_id: org,
            workspace_id: conv,
        },
    )
    .await?;
    if status["exists"] == true {
        return Ok(false);
    }
    let key = manifest["sha256"]
        .as_str()
        .context("manifest without checksum")?;
    let artifact = Uuid::now_v7();
    upload(app, org, key, artifact, &node).await?;
    let reply = sandbox::host_job(
        app,
        org,
        sandbox,
        HostCommand::WorkspaceRestore {
            org_id: org,
            workspace_id: conv,
            artifact,
            sha256: key.into(),
            disk_bytes,
        },
    )
    .await;
    let _ = sandbox::host_on(app, &node, HostCommand::DeleteArtifact { artifact }).await;
    Ok(reply?["restored"] == true)
}

/// Restore `manifest` into the new sandbox `spec` (a fork or an idle resume). The timeline is
/// unchanged: nothing happened after this checkpoint.
pub async fn restore_into(
    app: &App,
    lease: &Lease,
    spec: SandboxSpec,
    manifest: &Value,
) -> anyhow::Result<()> {
    let key = manifest["sha256"]
        .as_str()
        .context("manifest without checksum")?;
    ensure!(
        manifest["image"] == spec.image.as_str()
            && manifest["deps"].as_str() == spec.deps.as_deref(),
        Code::CheckpointIncompatible
    );
    ensure!(
        compatible(&app.config, manifest),
        Code::CheckpointIncompatible
    );
    let (org, artifact) = (lease.org_id, spec.id);
    let node = sandbox::node_of(app, org, artifact).await?;
    upload(app, org, key, artifact, &node).await?;
    sandbox::host_job(
        app,
        org,
        artifact,
        HostCommand::Restore {
            spec,
            artifact,
            sha256: key.into(),
        },
    )
    .await?;
    let _ = sandbox::host(app, org, artifact, HostCommand::DeleteArtifact { artifact }).await;
    Ok(())
}

/// Recover the workspace from its head (or pinned) checkpoint into a new sandbox: fence and
/// stop the old writer, restore, then mark every timeline item after the checkpoint's
/// watermark invalidated. Each step is idempotent, so a failed job is retried with the same
/// destination (the job id) and never applied twice.
pub async fn restore(app: &App, lease: &Lease, job: Uuid, link: &OwnerLink) -> anyhow::Result<()> {
    let (checkpoint, manifest, seq, old) = {
        let mut tx = app.store.fenced(lease).await?;
        let progress: (Option<Uuid>, Option<Uuid>, bool) = sqlx::query_as(
            "SELECT restore_checkpoint_id,restore_source_id,restore_applied FROM workspace_jobs
             WHERE org_id=$1 AND id=$2 AND conversation_id=$3 AND kind='restore' FOR UPDATE",
        )
        .bind(lease.org_id)
        .bind(job)
        .bind(lease.conversation_id)
        .fetch_one(&mut *tx)
        .await?;
        let (pinned, source, applied) = progress;
        if applied {
            tx.commit().await?;
            return Ok(());
        }
        let row = sqlx::query(
            "SELECT c.id,c.manifest,c.channel_seq FROM workspace_heads w
             JOIN checkpoints c ON c.org_id=w.org_id AND c.id=coalesce($3,w.checkpoint_id)
             WHERE w.org_id=$1 AND w.conversation_id=$2 AND c.state='committed' FOR UPDATE OF c,w",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .bind(pinned)
        .fetch_optional(&mut *tx)
        .await?
        .context(Code::NoCheckpoint)?;
        let checkpoint: Uuid = row.get("id");
        let manifest: Value = row.get("manifest");
        // The source is recorded once: a retry must never fence its own destination.
        let old: Uuid = match source {
            Some(source) => source,
            None => sqlx::query_scalar(
                "SELECT id FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent AND id<>$3
                 ORDER BY created_at DESC,id DESC LIMIT 1",
            )
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .bind(job)
            .fetch_one(&mut *tx)
            .await?,
        };
        ensure!(
            compatible(&app.config, &manifest),
            Code::CheckpointIncompatible
        );
        sqlx::query("INSERT INTO restore_pins(id,org_id,checkpoint_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(job)
            .bind(lease.org_id)
            .bind(checkpoint)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE workspace_jobs SET restore_checkpoint_id=$3,restore_source_id=$4 WHERE org_id=$1 AND id=$2",
        )
        .bind(lease.org_id)
        .bind(job)
        .bind(checkpoint)
        .bind(old)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE workspace_heads SET recovery_required=true WHERE org_id=$1 AND conversation_id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        (checkpoint, manifest, row.get::<i64, _>("channel_seq"), old)
    };
    fence_old_writer(app, lease, old).await?;
    let key = manifest["sha256"]
        .as_str()
        .context("manifest without checksum")?;
    // The destination row is written only after the upload succeeds, so a failed upload
    // leaves no blank workspace; a retry keeps the node an earlier attempt recorded.
    let node = {
        let mut tx = app.store.fenced(lease).await?;
        let recorded: Option<String> =
            sqlx::query_scalar("SELECT node FROM sandboxes WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(job)
                .fetch_optional(&mut *tx)
                .await?;
        let node = match recorded {
            Some(node) => node,
            None => sandbox::place(&mut tx, app, lease.conversation_id).await?,
        };
        tx.commit().await?;
        node
    };
    upload(app, lease.org_id, key, job, &node).await?;
    let spec = {
        let mut tx = app.store.fenced(lease).await?;
        let run: Uuid =
            sqlx::query_scalar("SELECT run_id FROM sandboxes WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(old)
                .fetch_one(&mut *tx)
                .await?;
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT lease_epoch FROM sandboxes WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(job)
                .fetch_optional(&mut *tx)
                .await?;
        let generation = match existing {
            Some(generation) => generation,
            None => sqlx::query_scalar(
                "UPDATE workspace_heads SET generation=generation+1 WHERE org_id=$1 AND conversation_id=$2 RETURNING generation",
            )
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut *tx)
            .await?,
        };
        let cfg = &app.config.sandbox;
        let limits = sandbox::limits(&mut tx, app, lease.org_id).await?;
        let spec = SandboxSpec {
            id: job,
            org_id: lease.org_id,
            workspace_id: lease.conversation_id,
            exec_epoch: lease.epoch as u64,
            image: manifest["image"]
                .as_str()
                .context("manifest without image")?
                .into(),
            runtime: cfg.runtime.clone(),
            memory_bytes: cfg.memory_bytes.max(512 * 1024 * 1024),
            pids_limit: cfg.pids_limit,
            persistent: true,
            egress: limits.egress,
            disk_bytes: limits.disk_bytes,
            // A workspace comes back on the layers it was checkpointed on.
            deps: manifest["deps"].as_str().map(Into::into),
        };
        sqlx::query(
            "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent,native_thread_id,node)
             VALUES($1,$2,$3,$4,$5,$6,'provisioning',$7,true,$8,$9) ON CONFLICT DO NOTHING",
        )
        .bind(job)
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .bind(run)
        .bind(generation)
        .bind(lease.epoch)
        .bind(serde_json::to_value(&spec)?)
        .bind(manifest["thread_id"].as_str())
        .bind(&node)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        spec
    };
    sandbox::host_job(
        app,
        lease.org_id,
        job,
        HostCommand::Restore {
            spec,
            artifact: job,
            sha256: key.into(),
        },
    )
    .await?;
    apply(app, lease, job, checkpoint, seq, &manifest).await?;
    link.committed().await;
    let _ = sandbox::host(
        app,
        lease.org_id,
        job,
        HostCommand::DeleteArtifact { artifact: job },
    )
    .await;
    Ok(())
}

/// Only a confirmed stop, or an expired node lease, allows a new writer for the workspace.
async fn fence_old_writer(app: &App, lease: &Lease, old: Uuid) -> anyhow::Result<()> {
    let stopped = sandbox::host(
        app,
        lease.org_id,
        old,
        HostCommand::Stop {
            sandbox_id: old,
            exec_epoch: lease.epoch as u64,
        },
    )
    .await
    .inspect_err(|error| tracing::warn!(sandbox = %old, error = format!("{error:#}"), "old writer not stopped"))
    .is_ok();
    let mut tx = app.store.fenced(lease).await?;
    let expired: bool = sqlx::query_scalar(
        "SELECT state='lost' OR coalesce(node_lease_until<=clock_timestamp(),false) FROM sandboxes
         WHERE org_id=$1 AND id=$2 FOR UPDATE",
    )
    .bind(lease.org_id)
    .bind(old)
    .fetch_one(&mut *tx)
    .await?;
    ensure!(stopped || expired, Code::OldWriterNotFenced);
    sqlx::query("UPDATE sandboxes SET state='lost',lease_epoch=lease_epoch+1 WHERE org_id=$1 AND id=$2 AND state<>'lost'")
        .bind(lease.org_id)
        .bind(old)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE virtual_keys SET revoked_at=now() WHERE org_id=$1 AND sandbox_id=$2")
        .bind(lease.org_id)
        .bind(old)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Commit the recovery: runs that were active fail, items beyond the watermark are marked
/// invalidated (kept, not deleted), and a notice explains what happened.
async fn apply(
    app: &App,
    lease: &Lease,
    job: Uuid,
    checkpoint: Uuid,
    seq: i64,
    manifest: &Value,
) -> anyhow::Result<()> {
    let mut tx = app.store.fenced(lease).await?;
    let affected: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM items WHERE org_id=$1 AND conversation_id=$2 AND last_seq>$3 AND kind<>'notice'",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(seq)
    .fetch_all(&mut *tx)
    .await?;
    let active: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('running','requires_action')",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_all(&mut *tx)
    .await?;
    for run in active {
        runs::finish(
            &mut tx,
            lease,
            run,
            "failed",
            Some(Code::CheckpointRecoveryRpo),
        )
        .await?;
    }
    for id in affected {
        let mut item = load_item(&mut tx, lease.org_id, id).await?;
        item.revision += 1;
        item.state = "invalidated".into();
        upsert_item(&mut tx, lease, item).await?;
    }
    sqlx::query(
        "UPDATE sandboxes SET state='running',exec_epoch=$3,node_lease_until=now()+interval '15 seconds' WHERE org_id=$1 AND id=$2",
    )
    .bind(lease.org_id)
    .bind(job)
    .bind(lease.epoch)
    .execute(&mut *tx)
    .await?;
    let archived: bool =
        sqlx::query_scalar("SELECT state='archived' FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut *tx)
            .await?;
    crate::lifecycle::unarchive(&mut tx, lease.org_id, lease.conversation_id, "hot").await?;
    if archived {
        crate::store::publish_conversation(&mut tx, lease.org_id, lease.conversation_id).await?;
    }
    let run: Uuid = sqlx::query_scalar("SELECT run_id FROM sandboxes WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(job)
        .fetch_one(&mut *tx)
        .await?;
    let excluded = manifest["excluded"].as_array().map_or(0, Vec::len);
    let mut text = format!(
        "工作区已恢复到检查点 {checkpoint}（时间线水位 {seq}）。水位后的操作已标记失效；请确认后继续。"
    );
    if excluded > 0 {
        text.push_str(&format!(
            "\n检查点不保存密钥类文件（.env、.ssh、auth.json），有 {excluded} 个需要重新配置。"
        ));
    }
    append(
        &mut tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        "",
        Event::ItemUpserted(Item {
            id: Uuid::now_v7().to_string(),
            revision: 1,
            kind: "notice".into(),
            state: "completed".into(),
            text,
            run_id: run.to_string(),
            ..Default::default()
        }),
    )
    .await?;
    sqlx::query("DELETE FROM restore_pins WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(job)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE workspace_jobs SET restore_applied=true WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(job)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE workspace_heads SET recovery_required=false WHERE org_id=$1 AND conversation_id=$2",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
