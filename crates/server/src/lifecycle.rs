//! Conversation lifecycle by policy. A conversation idle for `auto_archive_after_secs` is
//! archived (a Work workspace through its archive job, which checkpoints first); the stored
//! workspace of one archived for `archive_retention_secs` is deleted. The timeline stays.
//!
//! Runs in the Owner role: candidates are found with the read-only worker role, then each
//! change is made in the organization's own transaction under the conversation's channel lock,
//! which also serializes it with command admission, unarchiving and workspace jobs.
use std::time::Duration;

use ryzhub_execution::HostCommand;
use uuid::Uuid;

use crate::{
    app::App,
    store::{Tx, audit_as, effective_policy, publish_conversation},
};

const SWEEP_INTERVAL: Duration = Duration::from_secs(300);
/// Conversations handled per organization and sweep.
const BATCH: i64 = 100;
/// An automatic archive that failed is not tried again for this long.
const FAILED_ARCHIVE_RETRY_SECS: i64 = 24 * 3600;

/// Idle for `after` seconds: no run, queued command or workspace job in that time. A pinned
/// or temporary conversation is never archived automatically.
const IDLE: &str = "c.state='active' AND NOT c.temporary AND c.pinned_at IS NULL
    AND c.created_at<now()-($2::bigint*interval '1 second')
    AND NOT EXISTS(SELECT 1 FROM runs r WHERE r.org_id=c.org_id AND r.conversation_id=c.id
        AND coalesce(r.ended_at,'infinity')>now()-($2::bigint*interval '1 second'))
    AND NOT EXISTS(SELECT 1 FROM commands m WHERE m.org_id=c.org_id AND m.conversation_id=c.id
        AND m.status='queued')
    AND NOT EXISTS(SELECT 1 FROM workspace_jobs j WHERE j.org_id=c.org_id AND j.conversation_id=c.id
        AND (j.state IN ('queued','running')
             OR (j.kind='archive' AND j.state='failed'
                 AND j.created_at>now()-($3::bigint*interval '1 second'))))";

/// Archived, with its last run ended, more than `keep` seconds ago, and a stored workspace
/// not yet deleted.
const EXPIRED: &str = "c.state='archived'
    AND greatest(c.archived_at,(SELECT max(coalesce(r.ended_at,'infinity')) FROM runs r
        WHERE r.org_id=c.org_id AND r.conversation_id=c.id))<now()-($2::bigint*interval '1 second')
    AND EXISTS(SELECT 1 FROM workspace_heads h WHERE h.org_id=c.org_id AND h.conversation_id=c.id
        AND h.storage_state<>'deleted')";

pub async fn collector(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    loop {
        if let Err(error) = sweep(&app, &worker).await {
            tracing::warn!(
                error = format!("{error:#}"),
                "conversation lifecycle deferred"
            );
        }
        tokio::time::sleep(SWEEP_INTERVAL).await;
    }
}

pub async fn sweep(app: &App, worker: &sqlx::PgPool) -> anyhow::Result<()> {
    let instance = &app.config.policy;
    let configured =
        instance.auto_archive_after_secs.is_some() || instance.archive_retention_secs.is_some();
    // An organization's own policy may set either even when the instance does not.
    let orgs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT c.org_id FROM conversations c
         WHERE c.state IN ('active','archived') AND NOT c.temporary
           AND ($1 OR EXISTS(SELECT 1 FROM org_policies p WHERE p.org_id=c.org_id
               AND (p.policy ? 'auto_archive_after_secs' OR p.policy ? 'archive_retention_secs')))",
    )
    .bind(configured)
    .fetch_all(worker)
    .await?;
    for org in orgs {
        if let Err(error) = sweep_org(app, org).await {
            tracing::warn!(%org, error = format!("{error:#}"), "conversation lifecycle deferred");
        }
    }
    Ok(())
}

pub async fn sweep_org(app: &App, org: Uuid) -> anyhow::Result<()> {
    let policy = {
        let mut tx = app.store.tx(org).await?;
        let policy = effective_policy(&mut tx, &app.config.policy, org).await?;
        tx.commit().await?;
        policy
    };
    if let Some(after) = policy.auto_archive_after_secs.filter(|secs| *secs > 0) {
        let idle: Vec<Uuid> = {
            let mut tx = app.store.tx(org).await?;
            let idle = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT c.id FROM conversations c WHERE c.org_id=$1 AND {IDLE} LIMIT {BATCH}"
            )))
            .bind(org)
            .bind(after as i64)
            .bind(FAILED_ARCHIVE_RETRY_SECS)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            idle
        };
        for conv in idle {
            archive_idle(app, org, conv, after).await?;
        }
    }
    if let Some(keep) = policy.archive_retention_secs.filter(|secs| *secs > 0) {
        let expired: Vec<Uuid> = {
            let mut tx = app.store.tx(org).await?;
            let expired = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT c.id FROM conversations c WHERE c.org_id=$1 AND {EXPIRED} LIMIT {BATCH}"
            )))
            .bind(org)
            .bind(keep as i64)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            expired
        };
        for conv in expired {
            purge(app, org, conv, keep).await?;
        }
    }
    Ok(())
}

/// Lock the conversation's channel: command admission, settings and workspace jobs take it too.
async fn lock(tx: &mut Tx<'_>, org: Uuid, conv: Uuid) -> anyhow::Result<bool> {
    Ok(
        sqlx::query("SELECT id FROM channels WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE")
            .bind(org)
            .bind(conv)
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
    )
}

/// Archive `conv` if it is still idle. A Work workspace that is running or suspended is
/// archived by its job, which checkpoints and stops it; anything else is archived here.
async fn archive_idle(app: &App, org: Uuid, conv: Uuid, after: u64) -> anyhow::Result<()> {
    let mut tx = app.store.tx(org).await?;
    if !lock(&mut tx, org, conv).await? {
        return Ok(());
    }
    let mode: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT c.mode FROM conversations c WHERE c.org_id=$1 AND c.id=$4 AND {IDLE}"
    )))
    .bind(org)
    .bind(after as i64)
    .bind(FAILED_ARCHIVE_RETRY_SECS)
    .bind(conv)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(mode) = mode else {
        return Ok(());
    };
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent
         AND state IN ('running','provisioning','suspended'))",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&mut *tx)
    .await?;
    let creator: Uuid =
        sqlx::query_scalar("SELECT creator_id FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(conv)
            .fetch_one(&mut *tx)
            .await?;
    if mode == "work" && live && app.config.work.enabled {
        sqlx::query(
            "INSERT INTO workspace_jobs(id,org_id,conversation_id,kind) VALUES($1,$2,$3,'archive')
             ON CONFLICT DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(org)
        .bind(conv)
        .execute(&mut *tx)
        .await?;
    } else {
        archive(&mut tx, org, conv).await?;
        publish_conversation(&mut tx, org, conv).await?;
    }
    audit_as(
        &mut tx,
        org,
        creator,
        "system",
        "conversation.auto_archive",
        &conv.to_string(),
        "ok",
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Mark `conv` archived; the caller publishes it. Its stored workspace, if any, is now cold:
/// only the checkpoint store has it.
pub async fn archive(tx: &mut Tx<'_>, org: Uuid, conv: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE conversations SET state='archived',archived_at=now() WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(conv)
    .execute(&mut **tx)
    .await?;
    storage(tx, org, conv, "cold").await
}

/// Mark `conv` active again, with its workspace `storage_state`; the caller publishes it.
pub async fn unarchive(
    tx: &mut Tx<'_>,
    org: Uuid,
    conv: Uuid,
    storage_state: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE conversations SET state='active',archived_at=NULL WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(conv)
    .execute(&mut **tx)
    .await?;
    storage(tx, org, conv, storage_state).await
}

/// Record where `conv`'s workspace files are. A deleted workspace stays deleted until a new
/// checkpoint is committed; one being purged only becomes deleted.
pub async fn storage(tx: &mut Tx<'_>, org: Uuid, conv: Uuid, state: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE workspace_heads SET storage_state=$3 WHERE org_id=$1 AND conversation_id=$2
         AND (storage_state NOT IN ('purging','deleted') OR ($3='deleted' AND storage_state='purging'))",
    )
    .bind(org)
    .bind(conv)
    .bind(state)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Delete the stored workspace of `conv`, archived for `keep` seconds. Its checkpoints are
/// retired and reclaimed through the checkpoint collector's rules: an object another
/// checkpoint shares or a restore pins stays until it is free, and the workspace stays
/// `purging` until every checkpoint is gone.
pub async fn purge(app: &App, org: Uuid, conv: Uuid, keep: u64) -> anyhow::Result<()> {
    let retired: Vec<Uuid> = {
        let mut tx = app.store.tx(org).await?;
        if !lock(&mut tx, org, conv).await? {
            return Ok(());
        }
        let expired: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS(SELECT 1 FROM conversations c WHERE c.org_id=$1 AND c.id=$3 AND {EXPIRED})"
        )))
        .bind(org)
        .bind(keep as i64)
        .bind(conv)
        .fetch_one(&mut *tx)
        .await?;
        // A restore or archive in progress uses the head; a running sandbox has the files.
        let busy: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2
                           AND state IN ('queued','running'))
             OR EXISTS(SELECT 1 FROM sandboxes WHERE org_id=$1 AND conversation_id=$2
                       AND state IN ('running','provisioning'))",
        )
        .bind(org)
        .bind(conv)
        .fetch_one(&mut *tx)
        .await?;
        if !expired || busy {
            return Ok(());
        }
        // A node may still hold a Chat workspace it ran; a later run would pick that up.
        let held: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspace_heads WHERE org_id=$1 AND conversation_id=$2
             AND evicted_at IS NULL)
             AND EXISTS(SELECT 1 FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND NOT persistent)",
        )
        .bind(org)
        .bind(conv)
        .fetch_one(&mut *tx)
        .await?;
        if held {
            let evict = HostCommand::EvictWorkspace {
                org_id: org,
                workspace_id: conv,
            };
            if let Err(error) = crate::sandbox::host_workspace(app, org, conv, evict).await {
                tracing::warn!(%conv, error = format!("{error:#}"), "purged workspace not evicted");
                return Ok(());
            }
            sqlx::query(
                "UPDATE workspace_heads SET evicted_at=now() WHERE org_id=$1 AND conversation_id=$2",
            )
            .bind(org)
            .bind(conv)
            .execute(&mut *tx)
            .await?;
        }
        let first: bool = sqlx::query_scalar(
            "SELECT storage_state<>'purging' FROM workspace_heads
             WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE",
        )
        .bind(org)
        .bind(conv)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE workspace_heads SET checkpoint_id=NULL,storage_state='purging'
             WHERE org_id=$1 AND conversation_id=$2",
        )
        .bind(org)
        .bind(conv)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE checkpoints SET state='tombstoned',delete_after=now() WHERE org_id=$1
             AND conversation_id=$2 AND state IN ('committed','uploading')",
        )
        .bind(org)
        .bind(conv)
        .execute(&mut *tx)
        .await?;
        let retired = sqlx::query_scalar(
            "SELECT id FROM checkpoints WHERE org_id=$1 AND conversation_id=$2 AND state='tombstoned'",
        )
        .bind(org)
        .bind(conv)
        .fetch_all(&mut *tx)
        .await?;
        if first {
            let creator: Uuid = sqlx::query_scalar(
                "SELECT creator_id FROM conversations WHERE org_id=$1 AND id=$2",
            )
            .bind(org)
            .bind(conv)
            .fetch_one(&mut *tx)
            .await?;
            audit_as(
                &mut tx,
                org,
                creator,
                "system",
                "workspace.purge",
                &conv.to_string(),
                "ok",
            )
            .await?;
        }
        tx.commit().await?;
        retired
    };
    for id in retired {
        crate::workspace::gc(app, org, id).await?;
    }
    let mut tx = app.store.tx(org).await?;
    let remaining: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM checkpoints WHERE org_id=$1 AND conversation_id=$2 AND state<>'deleted')",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&mut *tx)
    .await?;
    if !remaining {
        storage(&mut tx, org, conv, "deleted").await?;
    }
    tx.commit().await?;
    Ok(())
}
