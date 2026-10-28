//! Work workspaces: checkpoints, archive, restore, idle suspension and retention.
//!
//! A checkpoint is the sandbox's overlay upper layer as a content-addressed `tar.zst`, plus a
//! watermark (guest `gseq` and channel `seq`) tying the files to the timeline. The execution
//! node builds the archive; the Owner streams it to storage and commits the manifest.
pub mod blob;
mod checkpoint;
mod gc;
mod restore;

pub use checkpoint::{create, current, workspace as checkpoint_workspace};
pub use gc::{collect_org, collector, gc};
pub use restore::{restore, restore_into, workspace_into as restore_workspace};

use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use ryzhub_execution::HostCommand;
use serde_json::{Value, json};
use tokio::io::AsyncRead;
use uuid::Uuid;

use crate::{
    app::App,
    error::{ApiResult, Code, is_fenced},
    owner::OwnerLink,
    sandbox,
    store::{Lease, Tx, conversation_in, effective_policy},
};
use blob::BlobStore;

/// A failed job is retried after this long.
const RETRY_AFTER_SECS: i64 = 30;
/// While a turn runs, its workspace is checkpointed at least this often.
const CHECKPOINT_INTERVAL_SECS: i64 = 300;

/// The primary store and, if configured, the secondary one, which keeps a second copy of
/// everything written.
pub(crate) struct Stores {
    pub primary: BlobStore,
    secondary: Option<BlobStore>,
}

impl Stores {
    /// Store `key` in every store, `source` giving the content and its size anew for each.
    pub async fn put<R: AsyncRead + Unpin, F: Future<Output = anyhow::Result<(u64, R)>>>(
        &self,
        key: &str,
        mut source: impl FnMut() -> F,
    ) -> anyhow::Result<()> {
        for store in std::iter::once(&self.primary).chain(&self.secondary) {
            let (size, reader) = source().await?;
            store.put_stream(key, reader, size).await?;
        }
        Ok(())
    }

    /// A reader over `key` and its size, from the secondary store if the primary fails.
    pub async fn open(&self, key: &str) -> anyhow::Result<(u64, blob::Reader)> {
        match (self.primary.open(key).await, &self.secondary) {
            (Err(_), Some(secondary)) => secondary.open(key).await,
            (opened, _) => opened,
        }
    }

    /// The whole object, verified, from the secondary store if the primary fails.
    pub async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        match (self.primary.get(key).await, &self.secondary) {
            (Err(_), Some(secondary)) => secondary.get(key).await,
            (read, _) => read,
        }
    }

    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        for store in std::iter::once(&self.primary).chain(&self.secondary) {
            store.delete(key).await?;
        }
        Ok(())
    }

    /// Delete every object in both stores.
    pub async fn clear(&self) -> anyhow::Result<()> {
        for store in std::iter::once(&self.primary).chain(&self.secondary) {
            store.clear().await?;
        }
        Ok(())
    }
}

/// The checkpoint stores, scoped to `org`.
pub(crate) fn stores(app: &App, org: Uuid) -> anyhow::Result<Stores> {
    let store = |root: &str| anyhow::Ok(BlobStore::configured(root)?.scoped(org));
    Ok(Stores {
        primary: store(&app.config().work.storage_root)?,
        secondary: app
            .config()
            .work
            .secondary_root
            .as_deref()
            .map(store)
            .transpose()?,
    })
}

/// The stores of `org`'s file library.
pub(crate) fn attachment_stores(app: &App, org: Uuid) -> anyhow::Result<Stores> {
    let Stores { primary, secondary } = stores(app, org)?;
    Ok(Stores {
        primary: primary.within("attachments"),
        secondary: secondary.map(|store| store.within("attachments")),
    })
}

/// Serialize publication and reclamation of one tenant's content-addressed object.
async fn blob_lock(tx: &mut Tx<'_>, org: Uuid, key: &str) -> anyhow::Result<()> {
    crate::store::advisory_lock(tx, &format!("checkpoint:{org}:{key}")).await
}

/// `GET /orgs/{org}/conversations/{conv}/workspace`
pub async fn status(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    conversation_in(&mut tx, &ctx, conv).await?;
    let head: Option<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',c.id,'created_at',c.created_at,'channel_seq',c.channel_seq,
                'gseq',c.gseq,'manifest',c.manifest)
         FROM workspace_heads w JOIN checkpoints c ON c.org_id=w.org_id AND c.id=w.checkpoint_id
         WHERE w.org_id=$1 AND w.conversation_id=$2",
    )
    .bind(org)
    .bind(conv)
    .fetch_optional(&mut *tx)
    .await?;
    let jobs: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',id,'kind',kind,'state',state,'result',result)
         FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2 ORDER BY created_at DESC LIMIT 10",
    )
    .bind(org)
    .bind(conv)
    .fetch_all(&mut *tx)
    .await?;
    let sandbox: Option<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',id,'state',state,'lease_until',node_lease_until)
         FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent ORDER BY created_at DESC LIMIT 1",
    )
    .bind(org)
    .bind(conv)
    .fetch_optional(&mut *tx)
    .await?;
    let (recovery, storage): (bool, Option<String>) = sqlx::query_as(
        "SELECT coalesce(bool_or(recovery_required),false),max(storage_state)
         FROM workspace_heads WHERE org_id=$1 AND conversation_id=$2",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&mut *tx)
    .await?;
    let stale: bool = sqlx::query_scalar(concat!(
        "SELECT EXISTS(SELECT 1 FROM sandboxes s WHERE s.org_id=$2 AND s.conversation_id=$3 AND ",
        stale_checkpoint!(),
        ")"
    ))
    .bind(sandbox::CHECKPOINT_STALE_SECS)
    .bind(org)
    .bind(conv)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({"checkpoint":head,"jobs":jobs,"sandbox":sandbox,
        "recovery_required":recovery,"stale":stale,"storage_state":storage}),
    ))
}

#[derive(serde::Deserialize)]
pub struct JobInput {
    pub kind: String,
}

/// `POST /orgs/{org}/conversations/{conv}/workspace`: queue a checkpoint, archive or restore.
/// Returns the queued job; a failed restore is retried with its original destination.
pub async fn enqueue(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Json(input): Json<JobInput>,
) -> ApiResult<Json<Value>> {
    require!(
        app.config().work.enabled
            && matches!(input.kind.as_str(), "checkpoint" | "archive" | "restore"),
        Code::InvalidInput
    );
    let (ctx, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    let conversation = conversation_in(&mut tx, &ctx, conv).await?;
    require!(conversation.mode == "work", Code::WorkOnly);
    sqlx::query("SELECT id FROM channels WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE")
        .bind(org)
        .bind(conv)
        .fetch_one(&mut *tx)
        .await?;
    let purged: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspace_heads WHERE org_id=$1 AND conversation_id=$2
         AND storage_state IN ('purging','deleted'))",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&mut *tx)
    .await?;
    require!(!purged, Code::WorkspacePurged);
    if input.kind == "archive" {
        let busy: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('running','requires_action'))",
        )
        .bind(org)
        .bind(conv)
        .fetch_one(&mut *tx)
        .await?;
        require!(!busy, Code::WorkspaceBusy);
    }
    let pending: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id,kind FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('queued','running')",
    )
    .bind(org)
    .bind(conv)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((id, kind)) = pending {
        require!(kind == input.kind, Code::WorkspaceBusy);
        tx.commit().await?;
        return Ok(Json(json!({"id":id,"state":"queued"})));
    }
    if input.kind == "restore" {
        let retry: Option<Uuid> = sqlx::query_scalar(
            "SELECT j.id FROM workspace_jobs j
             JOIN workspace_heads h ON h.org_id=j.org_id AND h.conversation_id=j.conversation_id
             WHERE j.org_id=$1 AND j.conversation_id=$2 AND j.kind='restore' AND j.state='failed'
               AND NOT j.restore_applied AND h.recovery_required
             ORDER BY j.created_at DESC LIMIT 1",
        )
        .bind(org)
        .bind(conv)
        .fetch_optional(&mut *tx)
        .await?;
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

/// Which automatic job, if any, the workspace needs now.
async fn automatic_job(
    app: &App,
    tx: &mut Tx<'_>,
    lease: &Lease,
) -> anyhow::Result<Option<&'static str>> {
    let org = lease.org_id;
    let conv = lease.conversation_id;
    let lost: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sandboxes s JOIN workspace_heads h ON h.org_id=s.org_id AND h.conversation_id=s.conversation_id
         WHERE s.org_id=$1 AND s.conversation_id=$2 AND s.persistent AND s.state='running'
           AND s.node_lease_until<clock_timestamp() AND h.checkpoint_id IS NOT NULL)",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&mut **tx)
    .await?;
    if lost {
        return Ok(Some("restore"));
    }
    // After every completed turn, and periodically while a turn runs. Never while idle.
    let checkpoint: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sandboxes s WHERE s.org_id=$1 AND s.conversation_id=$2 AND s.persistent
             AND s.state='running' AND s.native_thread_id IS NOT NULL
             AND (EXISTS(SELECT 1 FROM runs r WHERE r.org_id=s.org_id AND r.sandbox_id=s.id AND r.state='completed'
                         AND NOT EXISTS(SELECT 1 FROM checkpoints c WHERE c.org_id=s.org_id
                             AND c.conversation_id=s.conversation_id AND c.state IN ('uploading','committed')
                             AND (c.manifest->>'captured_at_ms')::bigint>=extract(epoch FROM r.ended_at)*1000))
                  OR (s.created_at<now()-($3::bigint*interval '1 second')
                      AND EXISTS(SELECT 1 FROM runs r WHERE r.org_id=s.org_id AND r.conversation_id=s.conversation_id
                                 AND r.state IN ('running','requires_action') AND NOT r.read_only)
                      AND NOT EXISTS(SELECT 1 FROM checkpoints c WHERE c.org_id=s.org_id
                                 AND c.conversation_id=s.conversation_id
                                 AND c.created_at>now()-($3::bigint*interval '1 second')))))",
    )
    .bind(org)
    .bind(conv)
    .bind(CHECKPOINT_INTERVAL_SECS)
    .fetch_one(&mut **tx)
    .await?;
    if checkpoint {
        return Ok(Some("checkpoint"));
    }
    let policy = effective_policy(tx, &app.config().policy, org).await?;
    let idle = app.config().idle_suspend_secs(&policy);
    if idle == 0 {
        return Ok(None);
    }
    let suspend: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sandboxes s WHERE s.org_id=$1 AND s.conversation_id=$2 AND s.persistent
             AND s.state='running' AND s.created_at<now()-($3::bigint*interval '1 second')
             AND coalesce(s.terminal_seen_at<now()-interval '1 minute',true)
             AND NOT EXISTS(SELECT 1 FROM runs r WHERE r.org_id=s.org_id AND r.conversation_id=s.conversation_id
                            AND coalesce(r.ended_at,'infinity')>now()-($3::bigint*interval '1 second')))",
    )
    .bind(org)
    .bind(conv)
    .bind(idle as i64)
    .fetch_one(&mut **tx)
    .await?;
    Ok(suspend.then_some("suspend"))
}

/// Run the conversation's next workspace job, queuing an automatic one if needed.
pub async fn jobs(app: &App, lease: &Lease, link: &OwnerLink) -> anyhow::Result<()> {
    let mut tx = app.store.fenced(lease).await?;
    let visible: bool = sqlx::query_scalar(
        "SELECT state IN ('active','archived') FROM conversations WHERE org_id=$1 AND id=$2",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut *tx)
    .await?;
    if !visible {
        return Ok(());
    }
    let recent: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2
         AND (state IN ('queued','running') OR (state='failed' AND created_at>now()-($3::bigint*interval '1 second'))))",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(RETRY_AFTER_SECS)
    .fetch_one(&mut *tx)
    .await?;
    if !recent && let Some(kind) = automatic_job(app, &mut tx, lease).await? {
        sqlx::query(
            "INSERT INTO workspace_jobs(id,org_id,conversation_id,kind) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .bind(kind)
        .execute(&mut *tx)
        .await?;
    }
    let job: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id,kind FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('queued','running')
         ORDER BY created_at LIMIT 1 FOR UPDATE",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, kind)) = job else {
        tx.commit().await?;
        return Ok(());
    };
    sqlx::query("UPDATE workspace_jobs SET state='running' WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // Where the files were, to return to if the job fails.
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT storage_state FROM workspace_heads WHERE org_id=$1 AND conversation_id=$2",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_optional(&mut *tx)
    .await?;
    let moving = match kind.as_str() {
        "archive" => Some("archiving"),
        "restore" => Some("restoring"),
        _ => None,
    };
    if let Some(moving) = moving {
        crate::lifecycle::storage(&mut tx, lease.org_id, lease.conversation_id, moving).await?;
    }
    tx.commit().await?;
    let result = match kind.as_str() {
        "restore" => restore(app, lease, id, link).await,
        "archive" => stop_after_checkpoint(app, lease, id, "archived", link).await,
        "suspend" => stop_after_checkpoint(app, lease, id, "suspended", link).await,
        // A suspended workspace's head is already current.
        _ => current(app, lease, id, link).await.map(|_| ()),
    };
    if result.as_ref().is_err_and(is_fenced) {
        return result;
    }
    if let Err(error) = &result {
        tracing::warn!(job = %id, error = format!("{error:#}"), "workspace job failed");
    }
    let mut tx = app.store.fenced(lease).await?;
    if let (Err(_), Some(_), Some(stored)) = (&result, moving, &stored) {
        crate::lifecycle::storage(&mut tx, lease.org_id, lease.conversation_id, stored).await?;
    }
    sqlx::query("UPDATE workspace_jobs SET state=$3,result=$4 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(id)
        .bind(if result.is_ok() {
            "completed"
        } else {
            "failed"
        })
        .bind(json!({"error":result.err().map(|e| format!("{e:#}"))}))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    link.committed().await;
    Ok(())
}

/// Checkpoint, then stop the sandbox. `archived` also archives the conversation; `suspended`
/// leaves it active and the next message resumes from this checkpoint.
async fn stop_after_checkpoint(
    app: &App,
    lease: &Lease,
    job: Uuid,
    state: &str,
    link: &OwnerLink,
) -> anyhow::Result<()> {
    let Some(sandbox) = checkpoint::running_sandbox(app, lease).await? else {
        // Already suspended: checkpointed and stopped, with nothing changed since.
        let head = checkpoint::suspended_head(app, lease).await?;
        head.context("no running or suspended workspace")?;
        if state == "archived" {
            let mut tx = app.store.fenced(lease).await?;
            sqlx::query(
                "UPDATE sandboxes SET state='archived' WHERE org_id=$1 AND conversation_id=$2
                 AND persistent AND state='suspended'",
            )
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .execute(&mut *tx)
            .await?;
            archive_conversation(&mut tx, lease).await?;
            tx.commit().await?;
        }
        return Ok(());
    };
    create(app, lease, job, link).await?;
    {
        // Runs wait for a suspension and archiving refuses busy workspaces, so nothing should
        // be using the sandbox; never stop one that is.
        let mut tx = app.store.fenced(lease).await?;
        let in_use: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE org_id=$1 AND sandbox_id=$2
             AND state IN ('running','requires_action'))",
        )
        .bind(lease.org_id)
        .bind(sandbox)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        ensure!(!in_use, Code::WorkspaceBusy);
    }
    sandbox::host(
        app,
        lease.org_id,
        sandbox,
        HostCommand::Stop {
            sandbox_id: sandbox,
            exec_epoch: lease.epoch as u64,
        },
    )
    .await?;
    let mut tx = app.store.fenced(lease).await?;
    sqlx::query(
        "UPDATE sandboxes SET state=$3,lease_epoch=lease_epoch+1 WHERE org_id=$1 AND id=$2",
    )
    .bind(lease.org_id)
    .bind(sandbox)
    .bind(state)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE virtual_keys SET revoked_at=now() WHERE org_id=$1 AND sandbox_id=$2 AND revoked_at IS NULL")
        .bind(lease.org_id)
        .bind(sandbox)
        .execute(&mut *tx)
        .await?;
    if state == "archived" {
        archive_conversation(&mut tx, lease).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn archive_conversation(tx: &mut Tx<'_>, lease: &Lease) -> anyhow::Result<()> {
    crate::lifecycle::archive(tx, lease.org_id, lease.conversation_id).await?;
    crate::store::publish_conversation(tx, lease.org_id, lease.conversation_id).await
}
