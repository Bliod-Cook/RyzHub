//! Reclaiming deleted files. Content is shared by every entry of the organization with the
//! same address, so an object goes only with the last entry that holds it. A reclaimed entry
//! stays as a tombstone, so references to it answer "deleted".
use uuid::Uuid;

use super::content_lock;
use crate::{app::App, workspace::attachment_stores};

/// An upload that has not finished after this long was abandoned.
const ABANDONED_UPLOAD_HOURS: i64 = 24;

/// Reclaim one deleted entry's content unless another live entry still holds it.
pub async fn reclaim(app: &App, org: Uuid, id: Uuid) -> anyhow::Result<()> {
    let mut tx = app.store.tx(org).await?;
    let sha256: Option<String> = sqlx::query_scalar(
        "SELECT sha256 FROM attachments WHERE org_id=$1 AND id=$2 AND state='deleted'",
    )
    .bind(org)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(sha256) = sha256 else {
        return Ok(());
    };
    // Uploads commit their entry under this lock before storing, so none can start using the
    // object between this check and its deletion.
    content_lock(&mut tx, org, &sha256).await?;
    let still: Option<String> =
        sqlx::query_scalar("SELECT state FROM attachments WHERE org_id=$1 AND id=$2 FOR UPDATE")
            .bind(org)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if still.as_deref() != Some("deleted") {
        return Ok(());
    }
    let shared: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM attachments WHERE org_id=$1 AND sha256=$2
         AND state IN ('uploading','ready'))
         OR EXISTS(SELECT 1 FROM project_files WHERE org_id=$1 AND source_sha256=$2)",
    )
    .bind(org)
    .bind(&sha256)
    .fetch_one(&mut *tx)
    .await?;
    if !shared {
        attachment_stores(app, org)?.delete(&sha256).await?;
    }
    sqlx::query(
        "UPDATE attachments SET state='reclaimed',name='',model_data=NULL,text_content=NULL
         WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Give up abandoned uploads, then reclaim deleted entries.
pub async fn collect_org(app: &App, org: Uuid) -> anyhow::Result<()> {
    let mut tx = app.store.tx(org).await?;
    sqlx::query(
        "UPDATE attachments SET state='deleted',deleted_at=now(),model_data=NULL,text_content=NULL
         WHERE org_id=$1 AND state='uploading' AND created_at<now()-($2::bigint*interval '1 hour')",
    )
    .bind(org)
    .bind(ABANDONED_UPLOAD_HOURS)
    .execute(&mut *tx)
    .await?;
    let deleted: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM attachments WHERE org_id=$1 AND state='deleted' ORDER BY deleted_at LIMIT 100",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    for id in deleted {
        reclaim(app, org, id).await?;
    }
    super::uploads::collect_org(app, org).await
}

/// Owner-role loop: organizations are discovered with the read-only worker role, then each is
/// processed through its own tenant-scoped transactions.
pub async fn collector(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    loop {
        let orgs: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT org_id FROM attachments WHERE state='deleted'
             OR (state='uploading' AND created_at<now()-($1::bigint*interval '1 hour'))
             UNION SELECT org_id FROM uploads WHERE expires_at<now() OR state='aborted'",
        )
        .bind(ABANDONED_UPLOAD_HOURS)
        .fetch_all(&worker)
        .await?;
        for org in orgs {
            if let Err(error) = collect_org(&app, org).await {
                tracing::warn!(%org, %error, "attachment reclamation deferred");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}
