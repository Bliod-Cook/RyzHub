//! Deletion of expired operational rows. Runs in the Owner role: organizations with expired
//! rows are found with the read-only worker role, then rows are deleted in bounded batches
//! through each organization's own row-level-security scope.
use std::time::Duration;
use uuid::Uuid;

use crate::app::App;

const SWEEP_INTERVAL: Duration = Duration::from_secs(600);
/// Processed commands are kept this long: the idempotency window for retried cids.
const COMMAND_DAYS: u32 = 7;
/// Acknowledged guest events.
const GUEST_EVENT_DAYS: u32 = 7;
/// Expired, revoked or superseded gateway keys.
const GATEWAY_KEY_DAYS: u32 = 7;
/// Rows deleted per statement, so no sweep holds locks for long.
const BATCH: u64 = 5000;

/// Expiry predicates by table. Only fixed identifiers and integers are formatted into the SQL.
fn rules() -> [(&'static str, String); 4] {
    [
        (
            "commands",
            format!("status='done' AND created_at<now()-interval '{COMMAND_DAYS} days'"),
        ),
        (
            "guest_events",
            format!("created_at<now()-interval '{GUEST_EVENT_DAYS} days'"),
        ),
        (
            "virtual_keys",
            format!(
                "(revoked_at IS NOT NULL OR superseded_at IS NOT NULL OR expires_at<now())
                 AND coalesce(revoked_at,superseded_at,expires_at)<now()-interval '{GATEWAY_KEY_DAYS} days'"
            ),
        ),
        // Rate windows are per minute; a day of history is plenty.
        (
            "model_rate_limits",
            "minute<floor(extract(epoch FROM now())/60)::bigint-1440".to_owned(),
        ),
        // `usage_records` is partitioned; maintenance drops its expired months. A ctid is not
        // unique across partitions, so the batched delete below must not be used on it.
    ]
}

async fn sweep(app: &App, worker: &sqlx::PgPool) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM ws_tickets WHERE expires_at<now()")
        .execute(&app.store.pool)
        .await?;
    for (table, expired) in rules() {
        let discover = format!("SELECT DISTINCT org_id FROM {table} WHERE {expired} LIMIT 1000");
        let orgs: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(discover))
            .fetch_all(worker)
            .await?;
        let delete = format!(
            "DELETE FROM {table} WHERE ctid IN (SELECT ctid FROM {table} WHERE org_id=$1 AND {expired} LIMIT {BATCH})"
        );
        for org in orgs {
            loop {
                let mut tx = app.store.tx(org).await?;
                let deleted = sqlx::query(sqlx::AssertSqlSafe(delete.clone()))
                    .bind(org)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                tx.commit().await?;
                if deleted < BATCH {
                    break;
                }
            }
        }
    }
    Ok(())
}

pub async fn collector(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    loop {
        if let Err(error) = sweep(&app, &worker).await {
            tracing::warn!(error = format!("{error:#}"), "retention sweep deferred");
        }
        tokio::time::sleep(SWEEP_INTERVAL).await;
    }
}
