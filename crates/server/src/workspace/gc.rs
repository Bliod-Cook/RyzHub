//! Checkpoint retention and object reclamation.
//!
//! Checkpoints are retired (tombstoned) by age and by count, then deleted after a grace
//! period. The current head and every restore pin are always kept. An object is deleted only
//! when no other live checkpoint of the tenant has the same content address.
use anyhow::Context;
use serde_json::Value;
use uuid::Uuid;

use super::{blob_lock, stores};
use crate::app::App;

/// An upload that has not committed after this long was abandoned.
const ABANDONED_UPLOAD_HOURS: i64 = 24;

/// Delete one tombstoned checkpoint whose grace period has passed. Returns whether it was
/// deleted; a pinned or current checkpoint is left alone.
pub async fn gc(app: &App, org: Uuid, id: Uuid) -> anyhow::Result<bool> {
    let mut tx = app.store.tx(org).await?;
    let key: Option<String> = sqlx::query_scalar(
        "SELECT manifest->>'sha256' FROM checkpoints WHERE org_id=$1 AND id=$2 AND state='tombstoned' AND delete_after<=now()",
    )
    .bind(org)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(key) = key else {
        return Ok(false);
    };
    blob_lock(&mut tx, org, &key).await?;
    // The row lock serializes this with a concurrent restore pinning the checkpoint.
    let manifest: Option<Value> = sqlx::query_scalar(
        "SELECT manifest FROM checkpoints WHERE org_id=$1 AND id=$2 AND state='tombstoned' AND delete_after<=now() FOR UPDATE",
    )
    .bind(org)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(manifest) = manifest else {
        return Ok(false);
    };
    let used: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM restore_pins WHERE org_id=$1 AND checkpoint_id=$2)
         OR EXISTS(SELECT 1 FROM workspace_heads WHERE org_id=$1 AND checkpoint_id=$2)",
    )
    .bind(org)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if used {
        return Ok(false);
    }
    let key = manifest["sha256"]
        .as_str()
        .context("manifest without checksum")?;
    let shared: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM checkpoints WHERE org_id=$1 AND id<>$2 AND state<>'deleted' AND manifest->>'sha256'=$3)",
    )
    .bind(org)
    .bind(id)
    .bind(key)
    .fetch_one(&mut *tx)
    .await?;
    if !shared {
        stores(app, org)?.delete(key).await?;
    }
    sqlx::query("UPDATE checkpoints SET state='deleted' WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// Retire checkpoints beyond the configured age or count and reclaim expired ones.
pub async fn collect_org(app: &App, org: Uuid) -> anyhow::Result<()> {
    let work = &app.config().work;
    let mut tx = app.store.tx(org).await?;
    sqlx::query(
        "UPDATE checkpoints c SET state='tombstoned',delete_after=now()+($4::bigint*interval '1 second')
         WHERE c.org_id=$1 AND c.state='committed'
           AND NOT EXISTS(SELECT 1 FROM workspace_heads h WHERE h.org_id=c.org_id AND h.checkpoint_id=c.id)
           AND NOT EXISTS(SELECT 1 FROM restore_pins p WHERE p.org_id=c.org_id AND p.checkpoint_id=c.id)
           AND (($2::bigint>0 AND c.created_at<now()-($2::bigint*interval '1 day'))
                OR ($3::bigint>0 AND (SELECT count(*) FROM checkpoints n WHERE n.org_id=c.org_id
                        AND n.conversation_id=c.conversation_id AND n.state='committed'
                        AND n.created_at>c.created_at)>=$3::bigint))",
    )
    .bind(org)
    .bind(i64::from(work.checkpoint_retention_days))
    .bind(i64::from(work.checkpoint_keep_last))
    .bind(i64::from(work.checkpoint_gc_grace_secs))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE checkpoints SET state='tombstoned',delete_after=now() WHERE org_id=$1 AND state='uploading'
         AND created_at<now()-($2::bigint*interval '1 hour')",
    )
    .bind(org)
    .bind(ABANDONED_UPLOAD_HOURS)
    .execute(&mut *tx)
    .await?;
    let expired: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM checkpoints WHERE org_id=$1 AND state='tombstoned' AND delete_after<=now()
         ORDER BY delete_after LIMIT 100",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    for id in expired {
        gc(app, org, id).await?;
    }
    Ok(())
}

/// Owner-role loop: organizations are discovered with the read-only worker role, then each is
/// processed through its own tenant-scoped transactions.
pub async fn collector(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    loop {
        let work = &app.config().work;
        let retention = work.checkpoint_retention_days > 0 || work.checkpoint_keep_last > 0;
        let orgs: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT org_id FROM checkpoints WHERE state IN ('tombstoned','uploading') OR ($1 AND state='committed')",
        )
        .bind(retention)
        .fetch_all(&worker)
        .await?;
        for org in orgs {
            if let Err(error) = collect_org(&app, org).await {
                tracing::warn!(%org, %error, "checkpoint collection deferred");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}
