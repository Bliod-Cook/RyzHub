use crate::{
    api::{ApiResult, App, authenticate},
    blob::BlobStore,
    chat, sandbox,
    store::{Lease, context_in, conversation_in},
    work,
};
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use ryzhub_execution::{HostCommand, SandboxSpec};
use ryzhub_proto::{Item, event_envelope::Event};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

pub async fn status(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let user = authenticate(&app, &headers).await?.user_id;
    let mut tx = app.store.tx(org).await?;
    let ctx = context_in(&mut tx, user, org).await?;
    conversation_in(&mut tx, &ctx, conv).await?;
    let head:Option<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',c.id,'created_at',c.created_at,'channel_seq',c.channel_seq,'gseq',c.gseq,'manifest',c.manifest) FROM workspace_heads w JOIN checkpoints c ON c.org_id=w.org_id AND c.id=w.checkpoint_id WHERE w.org_id=$1 AND w.conversation_id=$2").bind(org).bind(conv).fetch_optional(&mut *tx).await?;
    let jobs:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'kind',kind,'state',state,'result',result) FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2 ORDER BY created_at DESC LIMIT 10").bind(org).bind(conv).fetch_all(&mut *tx).await?;
    let sandbox:Option<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'state',state,'lease_until',node_lease_until) FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent ORDER BY created_at DESC LIMIT 1").bind(org).bind(conv).fetch_optional(&mut *tx).await?;
    let recovery: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_heads WHERE org_id=$1 AND conversation_id=$2 AND recovery_required)")
        .bind(org).bind(conv).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(
        json!({"checkpoint":head,"jobs":jobs,"sandbox":sandbox,"recovery_required":recovery}),
    ))
}
#[derive(serde::Deserialize)]
pub struct JobInput {
    pub kind: String,
}
pub async fn enqueue(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Json(input): Json<JobInput>,
) -> ApiResult<Json<Value>> {
    crate::api::check(
        app.config.work.enabled
            && matches!(input.kind.as_str(), "checkpoint" | "archive" | "restore"),
        "INVALID_INPUT",
    )?;
    let user = authenticate(&app, &headers).await?.user_id;
    let mut tx = app.store.tx(org).await?;
    let ctx = context_in(&mut tx, user, org).await?;
    let conv_row = conversation_in(&mut tx, &ctx, conv).await?;
    crate::api::check(conv_row.mode == "work", "WORK_ONLY")?;
    sqlx::query("SELECT id FROM channels WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE")
        .bind(org)
        .bind(conv)
        .fetch_one(&mut *tx)
        .await?;
    if input.kind == "archive" {
        let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('running','requires_action'))").bind(org).bind(conv).fetch_one(&mut *tx).await?;
        crate::api::check(!busy, "WORKSPACE_BUSY")?;
    }
    let pending:Option<(Uuid,String)>=sqlx::query_as("SELECT id,kind FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('queued','running')").bind(org).bind(conv).fetch_optional(&mut *tx).await?;
    if let Some((id, kind)) = pending {
        crate::api::check(kind == input.kind, "WORKSPACE_BUSY")?;
        tx.commit().await?;
        return Ok(Json(json!({"id":id,"state":"queued"})));
    }
    if input.kind == "restore" {
        let retry: Option<Uuid> = sqlx::query_scalar("SELECT j.id FROM workspace_jobs j JOIN workspace_heads h ON h.org_id=j.org_id AND h.conversation_id=j.conversation_id WHERE j.org_id=$1 AND j.conversation_id=$2 AND j.kind='restore' AND j.state='failed' AND NOT j.restore_applied AND h.recovery_required ORDER BY j.created_at DESC LIMIT 1")
            .bind(org).bind(conv).fetch_optional(&mut *tx).await?;
        if let Some(id) = retry {
            sqlx::query(
                "UPDATE workspace_jobs SET state='queued',result=NULL WHERE org_id=$1 AND id=$2",
            )
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(Json(json!({"id":id,"state":"queued"})));
        }
    }
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspace_jobs(id,org_id,conversation_id,kind) VALUES($1,$2,$3,$4)")
        .bind(id)
        .bind(org)
        .bind(conv)
        .bind(&input.kind)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"id":id,"state":"queued"})))
}
fn stores(app: &App, org: Uuid) -> anyhow::Result<(BlobStore, Option<BlobStore>)> {
    Ok((
        BlobStore::configured(&app.config.work.storage_root)?.scoped(org),
        app.config
            .work
            .secondary_root
            .as_deref()
            .map(|v| BlobStore::configured(v).map(|s| s.scoped(org)))
            .transpose()?,
    ))
}
pub async fn create(app: &App, lease: &Lease, artifact: Uuid) -> anyhow::Result<Uuid> {
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    let row=sqlx::query("SELECT id,run_id,native_thread_id FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent AND state='running'").bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut *tx).await?;
    let id: Uuid = row.get("id");
    let run: Uuid = row.get("run_id");
    let thread: Option<String> = row.get("native_thread_id");
    tx.commit().await?;
    sandbox::guest(app, lease, id, ryzhub_execution::GuestCommand::Fence).await?;
    let mut manifest = sandbox::host(
        app,
        HostCommand::Checkpoint {
            sandbox_id: id,
            exec_epoch: lease.epoch as u64,
            artifact,
        },
    )
    .await?;
    let gseq = manifest["gseq"].as_u64().context("MISSING_WATERMARK")?;
    let (sender, _rx) = tokio::sync::mpsc::channel(1);
    loop {
        work::sync(app, lease, run, id, &sender, Some(gseq)).await?;
        let mut tx = app.store.tx(lease.org_id).await?;
        let ack: i64 =
            sqlx::query_scalar("SELECT acked_gseq FROM sandboxes WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        if ack as u64 >= gseq {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let mut bytes = vec![];
    loop {
        let chunk = sandbox::host(
            app,
            HostCommand::ReadArtifact {
                artifact,
                offset: bytes.len() as u64,
            },
        )
        .await?;
        if chunk["eof"] == true {
            break;
        }
        bytes.extend(STANDARD.decode(chunk["data"].as_str().context("INVALID_CHUNK")?)?);
        ensure!(bytes.len() <= 8 * 1024 * 1024 * 1024, "ARCHIVE_TOO_LARGE");
    }
    let key = manifest["sha256"]
        .as_str()
        .context("MISSING_CHECKSUM")?
        .to_owned();
    let mut tx = app.store.tx(lease.org_id).await?;
    blob_lock(&mut tx, lease.org_id, &key).await?;
    let (primary, secondary) = stores(app, lease.org_id)?;
    primary.put(&key, &bytes).await?;
    if let Some(secondary) = secondary {
        secondary.put(&key, &bytes).await?;
    }
    manifest["thread_id"] = json!(thread);
    let settings = crate::work::run_settings(app, lease, run).await?;
    manifest["harness"] = json!(settings.harness);
    manifest["harness_version"] = json!(if settings.harness == "claude" {
        "0.3.283"
    } else {
        "0.154.0"
    });
    if settings.harness == "codex" {
        manifest["codex_version"] = json!("0.154.0");
    }
    manifest["secondary"] = json!(app.config.work.secondary_root.is_some());
    chat::lock(&mut tx, lease).await?;
    let seq: i64 = sqlx::query_scalar(
        "SELECT channel_seq FROM guest_events WHERE org_id=$1 AND sandbox_id=$2 AND gseq=$3",
    )
    .bind(lease.org_id)
    .bind(id)
    .bind(gseq as i64)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO checkpoints(id,org_id,conversation_id,sandbox_id,manifest,channel_seq,gseq) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING").bind(artifact).bind(lease.org_id).bind(lease.conversation_id).bind(id).bind(&manifest).bind(seq).bind(gseq as i64).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO workspace_heads(org_id,conversation_id,checkpoint_id) VALUES($1,$2,$3) ON CONFLICT(org_id,conversation_id) DO UPDATE SET checkpoint_id=$3").bind(lease.org_id).bind(lease.conversation_id).bind(artifact).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(artifact)
}
pub async fn restore(app: &App, lease: &Lease, job: Uuid) -> anyhow::Result<()> {
    // Persistent pins deliberately have no TTL. Failed jobs retain their pin until explicitly repaired.
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    let progress: (Option<Uuid>, Option<Uuid>, bool) = sqlx::query_as("SELECT restore_checkpoint_id,restore_source_id,restore_applied FROM workspace_jobs WHERE org_id=$1 AND id=$2 AND conversation_id=$3 AND kind='restore' FOR UPDATE")
        .bind(lease.org_id).bind(job).bind(lease.conversation_id).fetch_one(&mut *tx).await?;
    if progress.2 {
        tx.commit().await?;
        return Ok(());
    }
    let row=sqlx::query("SELECT c.id,c.manifest,c.channel_seq FROM workspace_heads w JOIN checkpoints c ON c.org_id=w.org_id AND c.id=coalesce($3,w.checkpoint_id) WHERE w.org_id=$1 AND w.conversation_id=$2 AND c.state='committed' FOR UPDATE OF c,w").bind(lease.org_id).bind(lease.conversation_id).bind(progress.0).fetch_optional(&mut *tx).await?.context("NO_CHECKPOINT")?;
    let checkpoint: Uuid = row.get("id");
    let manifest: Value = row.get("manifest");
    let seq: i64 = row.get("channel_seq");
    // Persist the source once. A retry must never fence its own destination.
    let old = if let Some(source) = progress.1 {
        source
    } else {
        sqlx::query_scalar("SELECT id FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent AND id<>$3 ORDER BY created_at DESC,id DESC LIMIT 1")
            .bind(lease.org_id).bind(lease.conversation_id).bind(job).fetch_one(&mut *tx).await?
    };
    ensure!(
        manifest["image"]
            .as_str()
            .is_some_and(|i| i.contains("@sha256:"))
            && manifest["format"] == 1
            && manifest["encoding"] == "tar.zst"
            && ((manifest["harness"] == "claude" && manifest["harness_version"] == "0.3.283")
                || manifest["codex_version"] == "0.154.0"),
        "CHECKPOINT_INCOMPATIBLE"
    );
    sqlx::query(
        "INSERT INTO restore_pins(id,org_id,checkpoint_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING",
    )
    .bind(job)
    .bind(lease.org_id)
    .bind(checkpoint)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE workspace_jobs SET restore_checkpoint_id=$3,restore_source_id=$4 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id).bind(job).bind(checkpoint).bind(old).execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE workspace_heads SET recovery_required=true WHERE org_id=$1 AND conversation_id=$2",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    // Explicit stop confirmation or expiry must precede a new workspace writer.
    let stopped = sandbox::host(
        app,
        HostCommand::Stop {
            sandbox_id: old,
            exec_epoch: lease.epoch as u64,
        },
    )
    .await
    .is_ok();
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    let expired:bool=sqlx::query_scalar("SELECT state='lost' OR coalesce(node_lease_until<=clock_timestamp(),false) FROM sandboxes WHERE org_id=$1 AND id=$2 FOR UPDATE").bind(lease.org_id).bind(old).fetch_one(&mut *tx).await?;
    ensure!(stopped || expired, "OLD_WRITER_NOT_FENCED");
    sqlx::query("UPDATE sandboxes SET state='lost',lease_epoch=lease_epoch+1 WHERE org_id=$1 AND id=$2 AND state<>'lost'").bind(lease.org_id).bind(old).execute(&mut *tx).await?;
    sqlx::query("UPDATE virtual_keys SET revoked_at=now() WHERE org_id=$1 AND sandbox_id=$2")
        .bind(lease.org_id)
        .bind(old)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let key = manifest["sha256"].as_str().context("MISSING_CHECKSUM")?;
    let (primary, secondary) = stores(app, lease.org_id)?;
    let bytes = match primary.get(key).await {
        Ok(bytes) => bytes,
        Err(e) => {
            if let Some(secondary) = secondary {
                let b = secondary.get(key).await?;
                primary.put(key, &b).await?;
                b
            } else {
                return Err(e);
            }
        }
    };
    for (index, chunk) in bytes.chunks(256 * 1024).enumerate() {
        sandbox::host(
            app,
            HostCommand::WriteArtifact {
                artifact: job,
                offset: (index * 256 * 1024) as u64,
                data: STANDARD.encode(chunk),
            },
        )
        .await?;
    }
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    let run: Uuid = sqlx::query_scalar("SELECT run_id FROM sandboxes WHERE org_id=$1 AND id=$2")
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
    let generation = if let Some(generation) = existing {
        generation
    } else {
        sqlx::query_scalar("UPDATE workspace_heads SET generation=generation+1 WHERE org_id=$1 AND conversation_id=$2 RETURNING generation").bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut *tx).await?
    };
    let cfg = &app.config.sandbox;
    let spec = SandboxSpec {
        id: job,
        org_id: lease.org_id,
        workspace_id: lease.conversation_id,
        exec_epoch: lease.epoch as u64,
        image: manifest["image"]
            .as_str()
            .context("INVALID_MANIFEST")?
            .into(),
        runtime: cfg.runtime.clone(),
        memory_bytes: cfg.memory_bytes.max(512 * 1024 * 1024),
        pids_limit: cfg.pids_limit,
        persistent: true,
    };
    sqlx::query("INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent,native_thread_id) VALUES($1,$2,$3,$4,$5,$6,'provisioning',$7,true,$8) ON CONFLICT DO NOTHING").bind(job).bind(lease.org_id).bind(lease.conversation_id).bind(run).bind(generation).bind(lease.epoch).bind(serde_json::to_value(&spec)?).bind(manifest["thread_id"].as_str()).execute(&mut *tx).await?;
    tx.commit().await?;
    sandbox::host(
        app,
        HostCommand::Restore {
            spec,
            artifact: job,
            sha256: key.into(),
        },
    )
    .await?;
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    // Preserve the timeline; mark every operation whose final revision was beyond the checkpoint.
    let affected:Vec<Uuid>=sqlx::query_scalar("SELECT id FROM items WHERE org_id=$1 AND conversation_id=$2 AND last_seq>$3 AND kind<>'notice'").bind(lease.org_id).bind(lease.conversation_id).bind(seq).fetch_all(&mut *tx).await?;
    let runs:Vec<Uuid>=sqlx::query_scalar("SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('running','requires_action')").bind(lease.org_id).bind(lease.conversation_id).fetch_all(&mut *tx).await?;
    for run in runs {
        chat::finish(
            &mut tx,
            lease,
            run,
            "failed",
            Some("CHECKPOINT_RECOVERY_RPO"),
        )
        .await?;
    }
    for id in affected {
        let mut item = chat::commands::item(&mut tx, lease.org_id, id).await?;
        item.revision += 1;
        item.state = "invalidated".into();
        chat::committed_item(&mut tx, lease, item).await?;
    }
    sqlx::query("UPDATE sandboxes SET state='running',exec_epoch=$3,node_lease_until=now()+interval '15 seconds' WHERE org_id=$1 AND id=$2").bind(lease.org_id).bind(job).bind(lease.epoch).execute(&mut *tx).await?;
    sqlx::query("UPDATE conversations SET state='active' WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .execute(&mut *tx)
        .await?;
    crate::store::append(&mut tx,lease.org_id,&lease.channel(),Some(lease),"",Event::ItemUpserted(Item{id:Uuid::now_v7().to_string(),revision:1,kind:"notice".into(),state:"completed".into(),text:format!("工作区已恢复到检查点 {checkpoint}（时间线水位 {seq}）。水位后的操作已标记失效；请确认后继续。"),run_id:run.to_string(),..Default::default()})).await?;
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

pub async fn jobs(app: &App, lease: &Lease) -> anyhow::Result<()> {
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    let active: bool = sqlx::query_scalar(
        "SELECT state IN ('active','archived') FROM conversations WHERE org_id=$1 AND id=$2",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut *tx)
    .await?;
    if !active {
        return Ok(());
    }
    let existing:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2 AND (state IN ('queued','running') OR (state='failed' AND created_at>now()-interval '30 seconds')))").bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut *tx).await?;
    if !existing {
        let expired:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sandboxes s JOIN workspace_heads h ON h.org_id=s.org_id AND h.conversation_id=s.conversation_id WHERE s.org_id=$1 AND s.conversation_id=$2 AND s.persistent AND s.state='running' AND s.node_lease_until<clock_timestamp() AND h.checkpoint_id IS NOT NULL)").bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut *tx).await?;
        let due:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sandboxes s WHERE s.org_id=$1 AND s.conversation_id=$2 AND s.persistent AND s.state='running' AND s.native_thread_id IS NOT NULL AND s.created_at<now()-($3::bigint*interval '1 second') AND NOT EXISTS(SELECT 1 FROM checkpoints c WHERE c.org_id=s.org_id AND c.conversation_id=s.conversation_id AND c.created_at>now()-($3::bigint*interval '1 second')))").bind(lease.org_id).bind(lease.conversation_id).bind(app.config.work.checkpoint_interval_secs as i64).fetch_one(&mut *tx).await?;
        let finished_turn:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sandboxes s JOIN runs r ON r.org_id=s.org_id AND r.sandbox_id=s.id WHERE s.org_id=$1 AND s.conversation_id=$2 AND s.persistent AND s.state='running' AND r.state='completed' AND NOT EXISTS(SELECT 1 FROM checkpoints c WHERE c.org_id=s.org_id AND c.conversation_id=s.conversation_id AND (c.manifest->>'captured_at_ms')::bigint >= extract(epoch FROM r.ended_at)*1000))")
            .bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut *tx).await?;
        if expired || due || finished_turn {
            sqlx::query("INSERT INTO workspace_jobs(id,org_id,conversation_id,kind) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING").bind(Uuid::now_v7()).bind(lease.org_id).bind(lease.conversation_id).bind(if expired{"restore"}else{"checkpoint"}).execute(&mut *tx).await?;
        }
    }
    let row:Option<(Uuid,String)>=sqlx::query_as("SELECT id,kind FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('queued','running') ORDER BY created_at LIMIT 1 FOR UPDATE").bind(lease.org_id).bind(lease.conversation_id).fetch_optional(&mut *tx).await?;
    let Some((id, kind)) = row else {
        tx.commit().await?;
        return Ok(());
    };
    sqlx::query("UPDATE workspace_jobs SET state='running' WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let result=async {
  if kind=="restore" {restore(app,lease,id).await?;} else {
   create(app,lease,id).await?;
   if kind=="archive" {
    let mut tx=app.store.tx(lease.org_id).await?;let sandbox:Uuid=sqlx::query_scalar("SELECT id FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent AND state='running'").bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut *tx).await?;tx.commit().await?;
    sandbox::host(app,HostCommand::Stop{sandbox_id:sandbox,exec_epoch:lease.epoch as u64}).await?;
    let mut tx=app.store.tx(lease.org_id).await?;chat::lock(&mut tx,lease).await?;
    sqlx::query("UPDATE sandboxes SET state='archived',lease_epoch=lease_epoch+1 WHERE org_id=$1 AND id=$2").bind(lease.org_id).bind(sandbox).execute(&mut *tx).await?;
    sqlx::query("UPDATE conversations SET state='archived' WHERE org_id=$1 AND id=$2").bind(lease.org_id).bind(lease.conversation_id).execute(&mut *tx).await?;tx.commit().await?;
   }
  }Ok::<_,anyhow::Error>(())
 }.await;
    if result.as_ref().is_err_and(|e| e.to_string() == "FENCED") {
        return result;
    }
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    sqlx::query("UPDATE workspace_jobs SET state=$3,result=$4 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(id)
        .bind(if result.is_ok() {
            "completed"
        } else {
            "failed"
        })
        .bind(json!({"error":result.err().map(|e|e.to_string())}))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
// Serialize publication and reclamation of the same tenant's content-addressed object.
async fn blob_lock(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: Uuid,
    key: &str,
) -> anyhow::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("checkpoint:{org}:{key}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// The row lock serializes new restore pins and GC. Deletion never guesses about stale pins.
pub async fn gc(app: &App, org: Uuid, id: Uuid) -> anyhow::Result<bool> {
    let mut tx = app.store.tx(org).await?;
    let key:Option<String>=sqlx::query_scalar("SELECT manifest->>'sha256' FROM checkpoints WHERE org_id=$1 AND id=$2 AND state='tombstoned' AND delete_after<=now()").bind(org).bind(id).fetch_optional(&mut *tx).await?;
    let Some(key) = key else {
        return Ok(false);
    };
    blob_lock(&mut tx, org, &key).await?;
    let manifest:Option<Value>=sqlx::query_scalar("SELECT manifest FROM checkpoints WHERE org_id=$1 AND id=$2 AND state='tombstoned' AND delete_after<=now() FOR UPDATE").bind(org).bind(id).fetch_optional(&mut *tx).await?;
    let Some(manifest) = manifest else {
        return Ok(false);
    };
    let used:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM restore_pins WHERE org_id=$1 AND checkpoint_id=$2) OR EXISTS(SELECT 1 FROM workspace_heads WHERE org_id=$1 AND checkpoint_id=$2)").bind(org).bind(id).fetch_one(&mut *tx).await?;
    if used {
        return Ok(false);
    };
    let key = manifest["sha256"].as_str().context("INVALID_MANIFEST")?;
    let shared:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM checkpoints WHERE org_id=$1 AND id<>$2 AND state<>'deleted' AND manifest->>'sha256'=$3)").bind(org).bind(id).bind(key).fetch_one(&mut *tx).await?;
    if !shared {
        let (p, s) = stores(app, org)?;
        p.delete(key).await?;
        if let Some(s) = s {
            s.delete(key).await?;
        }
    }
    sqlx::query("UPDATE checkpoints SET state='deleted' WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// Retention is opt-in and only retires superseded checkpoints. The latest recovery point
/// and every restore pin survive both expiration and physical reclamation.
pub async fn collect_org(app: &App, org: Uuid) -> anyhow::Result<()> {
    let mut tx = app.store.tx(org).await?;
    if app.config.work.checkpoint_retention_days > 0 {
        sqlx::query("UPDATE checkpoints c SET state='tombstoned',delete_after=now()+($3::bigint*interval '1 second') WHERE c.org_id=$1 AND c.state='committed' AND c.created_at<now()-($2::bigint*interval '1 day') AND NOT EXISTS(SELECT 1 FROM workspace_heads h WHERE h.org_id=c.org_id AND h.checkpoint_id=c.id) AND NOT EXISTS(SELECT 1 FROM restore_pins p WHERE p.org_id=c.org_id AND p.checkpoint_id=c.id)")
            .bind(org).bind(i64::from(app.config.work.checkpoint_retention_days)).bind(i64::from(app.config.work.checkpoint_gc_grace_secs)).execute(&mut *tx).await?;
    }
    let expired:Vec<Uuid>=sqlx::query_scalar("SELECT id FROM checkpoints WHERE org_id=$1 AND state='tombstoned' AND delete_after<=now() ORDER BY delete_after LIMIT 100").bind(org).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    for id in expired {
        gc(app, org, id).await?;
    }
    Ok(())
}

pub async fn collector(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    loop {
        let orgs:Vec<Uuid>=sqlx::query_scalar("SELECT DISTINCT org_id FROM checkpoints WHERE state='tombstoned' OR ($1 AND state='committed')")
            .bind(app.config.work.checkpoint_retention_days>0).fetch_all(&worker).await?;
        for org in orgs {
            if let Err(error) = collect_org(&app, org).await {
                tracing::warn!(%org,%error,"checkpoint collection deferred");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}
/// Gateway-side liveness renewal survives a rolling restart of every Owner process.
pub async fn heartbeat(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    loop {
        let live = sandbox::host(&app, HostCommand::Status).await;
        if let Ok(live) = live {
            let ids: Vec<Uuid> = serde_json::from_value(live["running"].clone())?;
            let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT org_id,id FROM sandboxes WHERE persistent AND state='running'",
            )
            .fetch_all(&worker)
            .await?;
            for (org, id) in rows {
                if ids.contains(&id) {
                    let mut tx = app.store.tx(org).await?;
                    sqlx::query("UPDATE sandboxes SET node_lease_until=now()+interval '15 seconds' WHERE org_id=$1 AND id=$2 AND node_lease_until>clock_timestamp()").bind(org).bind(id).execute(&mut *tx).await?;
                    tx.commit().await?;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Forks restore the same durable upper into a distinct workspace before forking native history.
pub async fn restore_fork(
    app: &App,
    lease: &Lease,
    spec: ryzhub_execution::SandboxSpec,
    manifest: &Value,
) -> anyhow::Result<()> {
    let key = manifest["sha256"].as_str().context("INVALID_MANIFEST")?;
    ensure!(manifest["image"] == spec.image, "CHECKPOINT_IMAGE_MISMATCH");
    let (primary, secondary) = stores(app, lease.org_id)?;
    let bytes = match primary.get(key).await {
        Ok(v) => v,
        Err(e) => {
            if let Some(s) = secondary {
                s.get(key).await?
            } else {
                return Err(e);
            }
        }
    };
    for (index, chunk) in bytes.chunks(256 * 1024).enumerate() {
        sandbox::host(
            app,
            HostCommand::WriteArtifact {
                artifact: spec.id,
                offset: (index * 256 * 1024) as u64,
                data: STANDARD.encode(chunk),
            },
        )
        .await?;
    }
    let artifact = spec.id;
    sandbox::host(
        app,
        HostCommand::Restore {
            spec,
            artifact,
            sha256: key.into(),
        },
    )
    .await?;
    Ok(())
}
