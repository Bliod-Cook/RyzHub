//! Library uploads in parts. A file larger than [`PART_SIZE`] is sent as numbered parts of
//! that size, each retried or resumed on its own, then completed. When the primary store is an
//! S3 bucket the parts go straight to it over presigned URLs; otherwise each comes through the
//! gateway, hashed as it arrives, into the store. Either way the server reads the whole file
//! back, hashing it, before it joins the library like any upload.
use std::time::Duration;

use anyhow::Context;
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use super::{
    display_name,
    http::{UPLOADS, describe},
    ingest::{Spool, ingest},
    limit,
};
use crate::{
    app::App,
    error::{ApiResult, Code},
    http::member_tx,
    store::{Tx, effective_policy},
    workspace::{attachment_stores, blob::BlobStore},
};

/// Part size, and the size above which clients upload in parts.
pub const PART_SIZE: u64 = 8 * 1024 * 1024;
/// An upload not completed in this long is given up.
const EXPIRY_HOURS: i64 = 24;
/// How long a presigned part URL works; asking for the upload again gives fresh ones.
const PRESIGNED: Duration = Duration::from_secs(3600);

/// Where the parts of upload `id` wait: under their digests in a namespace of their own.
fn parts_store(app: &App, org: Uuid, id: Uuid) -> anyhow::Result<BlobStore> {
    Ok(attachment_stores(app, org)?
        .primary
        .within("uploads")
        .within(&id.to_string()))
}

/// The bucket object a direct upload is assembled into, and its key there.
pub(crate) fn direct_object(app: &App, org: Uuid, id: Uuid) -> anyhow::Result<(BlobStore, String)> {
    let key = format!("{:x}", Sha256::digest(format!("upload:{id}")));
    Ok((attachment_stores(app, org)?.primary.within("uploads"), key))
}

#[derive(sqlx::FromRow)]
struct Row {
    name: String,
    size: i64,
    part_size: i64,
    direct: bool,
    s3_upload_id: Option<String>,
    state: String,
    attachment_id: Option<Uuid>,
}

/// The caller's own upload, locked.
async fn mine(tx: &mut Tx<'_>, org: Uuid, user: Uuid, id: Uuid) -> anyhow::Result<Row> {
    let row: Option<Row> = sqlx::query_as(
        "SELECT name,size,part_size,direct,s3_upload_id,state,attachment_id FROM uploads
         WHERE org_id=$1 AND user_id=$2 AND id=$3 FOR UPDATE",
    )
    .bind(org)
    .bind(user)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    row.context(Code::UploadNotFound)
}

fn part_count(row: &Row) -> i64 {
    (row.size + row.part_size - 1) / row.part_size
}

/// The size part `n` must have.
fn part_length(row: &Row, n: i64) -> Option<u64> {
    let count = part_count(row);
    (1..=count).contains(&n).then(|| {
        if n < count {
            row.part_size as u64
        } else {
            (row.size - (count - 1) * row.part_size) as u64
        }
    })
}

/// What the client needs to go on: the parts received, and for a direct upload a URL for
/// each part still missing.
async fn status(
    app: &App,
    tx: &mut Tx<'_>,
    org: Uuid,
    id: Uuid,
    row: &Row,
) -> anyhow::Result<Value> {
    let received: Vec<i32> = sqlx::query_scalar(
        "SELECT n FROM upload_parts WHERE org_id=$1 AND upload_id=$2 ORDER BY n",
    )
    .bind(org)
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    let mut urls = vec![];
    if let (true, "open", Some(upload)) = (row.direct, row.state.as_str(), &row.s3_upload_id) {
        let (store, key) = direct_object(app, org, id)?;
        for n in 1..=part_count(row) {
            if !received.contains(&(n as i32)) {
                let url = store
                    .presign_part(&key, upload, n as usize, PRESIGNED)
                    .await?;
                urls.push(json!({"n": n, "url": url}));
            }
        }
    }
    Ok(json!({
        "id": id, "name": row.name, "size": row.size, "part_size": row.part_size,
        "parts": part_count(row), "direct": row.direct, "state": row.state,
        "received": received, "urls": urls, "attachment_id": row.attachment_id,
    }))
}

#[derive(Deserialize)]
pub struct Start {
    name: String,
    size: u64,
}

/// `POST /orgs/{org}/uploads`: begin uploading `size` bytes as `name` in parts.
pub async fn start(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(input): Json<Start>,
) -> ApiResult<Json<Value>> {
    let name = display_name(&input.name).context(Code::InvalidAttachmentName)?;
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let policy = effective_policy(&mut tx, &app.config().policy, org).await?;
    let limit = limit(&policy);
    require!(
        input.size > 0 && input.size <= limit,
        Code::AttachmentTooLarge
    );
    let id = Uuid::now_v7();
    let direct = attachment_stores(&app, org)?.primary.direct();
    let s3_upload_id = if direct {
        let (store, key) = direct_object(&app, org, id)?;
        Some(store.start_direct(&key).await?)
    } else {
        None
    };
    sqlx::query(
        "INSERT INTO uploads(id,org_id,user_id,name,size,part_size,direct,s3_upload_id,expires_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,now()+($9::bigint*interval '1 hour'))",
    )
    .bind(id)
    .bind(org)
    .bind(ctx.user_id)
    .bind(&name)
    .bind(input.size as i64)
    .bind(PART_SIZE as i64)
    .bind(direct)
    .bind(&s3_upload_id)
    .bind(EXPIRY_HOURS)
    .execute(&mut *tx)
    .await?;
    let row = mine(&mut tx, org, ctx.user_id, id).await?;
    let status = status(&app, &mut tx, org, id, &row).await?;
    tx.commit().await?;
    Ok(Json(status))
}

/// `GET /orgs/{org}/uploads/{id}`: where an upload stands, to resume it.
pub async fn show(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let row = mine(&mut tx, org, ctx.user_id, id).await?;
    let status = status(&app, &mut tx, org, id, &row).await?;
    tx.commit().await?;
    Ok(Json(status))
}

/// `PUT /orgs/{org}/uploads/{id}/parts/{n}`: part `n` (from 1) of an upload through the
/// gateway. Sending a part again replaces it.
pub async fn part(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id, n)): Path<(Uuid, Uuid, i64)>,
    body: Body,
) -> ApiResult<Json<Value>> {
    let (user, length) = {
        let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
        let row = mine(&mut tx, org, ctx.user_id, id).await?;
        tx.commit().await?;
        require!(row.state == "open", Code::UploadClosed);
        require!(!row.direct, Code::InvalidUploadPart);
        let length = part_length(&row, n).context(Code::InvalidUploadPart)?;
        (ctx.user_id, length)
    };
    let _permit = UPLOADS.try_acquire().map_err(|_| Code::UploadBusy)?;
    let spool = Spool::receive(body, length)
        .await
        .map_err(|_| Code::InvalidUploadPart)?;
    require!(spool.size == length, Code::InvalidUploadPart);
    let store = parts_store(&app, org, id)?;
    let file = tokio::fs::File::open(spool.path()).await?;
    store.put_stream(&spool.sha256, file, spool.size).await?;
    let mut tx = app.store.tx(org).await?;
    let row = mine(&mut tx, org, user, id).await?;
    require!(row.state == "open", Code::UploadClosed);
    let replaced: Option<Option<String>> = sqlx::query_scalar(
        "SELECT sha256 FROM upload_parts WHERE org_id=$1 AND upload_id=$2 AND n=$3",
    )
    .bind(org)
    .bind(id)
    .bind(n as i32)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO upload_parts(org_id,upload_id,n,size,sha256) VALUES($1,$2,$3,$4,$5)
         ON CONFLICT(upload_id,n) DO UPDATE SET size=EXCLUDED.size,sha256=EXCLUDED.sha256",
    )
    .bind(org)
    .bind(id)
    .bind(n as i32)
    .bind(spool.size as i64)
    .bind(&spool.sha256)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if let Some(Some(old)) = replaced.filter(|old| old.as_deref() != Some(&spool.sha256)) {
        let used: bool = {
            let mut tx = app.store.tx(org).await?;
            let used = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM upload_parts WHERE org_id=$1 AND upload_id=$2 AND sha256=$3)",
            )
            .bind(org)
            .bind(id)
            .bind(&old)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            used
        };
        if !used {
            store.delete(&old).await?;
        }
    }
    Ok(Json(json!({"n": n, "sha256": spool.sha256})))
}

#[derive(Deserialize)]
pub struct Sent {
    etag: String,
}

/// `POST /orgs/{org}/uploads/{id}/parts/{n}/sent`: part `n` of a direct upload reached the
/// bucket with entity tag `etag`.
pub async fn sent(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id, n)): Path<(Uuid, Uuid, i64)>,
    Json(input): Json<Sent>,
) -> ApiResult<Json<Value>> {
    require!(
        !input.etag.is_empty() && input.etag.len() <= 256,
        Code::InvalidUploadPart
    );
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let row = mine(&mut tx, org, ctx.user_id, id).await?;
    require!(row.state == "open", Code::UploadClosed);
    require!(row.direct, Code::InvalidUploadPart);
    let length = part_length(&row, n).context(Code::InvalidUploadPart)?;
    sqlx::query(
        "INSERT INTO upload_parts(org_id,upload_id,n,size,etag) VALUES($1,$2,$3,$4,$5)
         ON CONFLICT(upload_id,n) DO UPDATE SET etag=EXCLUDED.etag",
    )
    .bind(org)
    .bind(id)
    .bind(n as i32)
    .bind(length as i64)
    .bind(&input.etag)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"n": n})))
}

/// `POST /orgs/{org}/uploads/{id}/complete`: put the parts together and add the file to the
/// library. Completing again returns the same file.
pub async fn complete(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, policy, row, parts) = {
        let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
        let policy = effective_policy(&mut tx, &app.config().policy, org).await?;
        let row = mine(&mut tx, org, ctx.user_id, id).await?;
        if let (Some(file), "completed") = (row.attachment_id, row.state.as_str()) {
            let file = describe(&mut tx, org, ctx.user_id, file).await?;
            tx.commit().await?;
            return Ok(Json(file));
        }
        require!(row.state == "open", Code::UploadClosed);
        let parts: Vec<(i32, i64, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT n,size,sha256,etag FROM upload_parts WHERE org_id=$1 AND upload_id=$2 ORDER BY n",
        )
        .bind(org)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        require!(
            parts.len() as i64 == part_count(&row)
                && parts
                    .iter()
                    .all(|(n, size, ..)| part_length(&row, i64::from(*n)) == Some(*size as u64)),
            Code::UploadIncomplete
        );
        sqlx::query("UPDATE uploads SET state='completing' WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        (ctx, policy, row, parts)
    };
    let _permit = UPLOADS.try_acquire().map_err(|_| Code::UploadBusy);
    let result = async {
        let _permit = _permit?;
        let limit = limit(&policy);
        let size = row.size as u64;
        let spool = if row.direct {
            let (store, key) = direct_object(&app, org, id)?;
            let upload = row.s3_upload_id.as_deref().context("direct upload id")?;
            let etags = parts
                .iter()
                .map(|(.., etag)| etag.clone().context(Code::UploadIncomplete))
                .collect::<anyhow::Result<Vec<_>>>()?;
            store.finish_direct(&key, upload, etags).await?;
            let (stored, reader) = store.open(&key).await?;
            anyhow::ensure!(stored == size, Code::UploadIncomplete);
            Spool::read(reader, size, limit).await?
        } else {
            let store = parts_store(&app, org, id)?;
            let mut reader: crate::workspace::blob::Reader = Box::pin(tokio::io::empty());
            for (_, _, sha256, _) in &parts {
                let sha256 = sha256.as_deref().context(Code::UploadIncomplete)?;
                let (_, part) = store.open(sha256).await?;
                reader = Box::pin(reader.chain(part));
            }
            Spool::read(reader, size, limit).await?
        };
        ingest(
            &app,
            org,
            ctx.user_id,
            &row.name,
            "upload",
            &spool,
            policy.attachment_quota_bytes,
        )
        .await
    }
    .await;
    let mut tx = app.store.tx(org).await?;
    match result {
        Ok(file) => {
            sqlx::query(
                "UPDATE uploads SET state='completed',attachment_id=$3 WHERE org_id=$1 AND id=$2",
            )
            .bind(org)
            .bind(id)
            .bind(file)
            .execute(&mut *tx)
            .await?;
            let described = describe(&mut tx, org, ctx.user_id, file).await?;
            tx.commit().await?;
            discard(&app, org, id).await;
            Ok(Json(described))
        }
        Err(error) => {
            // A refusal (too large, over quota) ends the upload; anything else may be retried.
            let refused = matches!(
                error.downcast_ref::<Code>(),
                Some(Code::AttachmentTooLarge | Code::AttachmentQuotaExceeded)
            );
            sqlx::query("UPDATE uploads SET state=$3 WHERE org_id=$1 AND id=$2")
                .bind(org)
                .bind(id)
                .bind(if refused { "aborted" } else { "open" })
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            if refused {
                discard(&app, org, id).await;
            }
            Err(error.into())
        }
    }
}

/// `DELETE /orgs/{org}/uploads/{id}`: give up an upload.
pub async fn abort(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let row = mine(&mut tx, org, ctx.user_id, id).await?;
    require!(row.state != "completing", Code::UploadBusy);
    if row.state == "open" {
        sqlx::query("UPDATE uploads SET state='aborted' WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    discard(&app, org, id).await;
    Ok(Json(json!({"aborted": row.state != "completed"})))
}

/// Delete what an upload left in the store: its parts, or its direct upload and the object
/// assembled from it. Failures are logged; the collector tries again until the row expires.
async fn discard(app: &App, org: Uuid, id: Uuid) {
    let result = async {
        let mut tx = app.store.tx(org).await?;
        let (direct, upload): (bool, Option<String>) =
            sqlx::query_as("SELECT direct,s3_upload_id FROM uploads WHERE org_id=$1 AND id=$2")
                .bind(org)
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        let parts: Vec<String> = sqlx::query_scalar(
            "SELECT sha256 FROM upload_parts WHERE org_id=$1 AND upload_id=$2 AND sha256 IS NOT NULL",
        )
        .bind(org)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        if direct {
            let (store, key) = direct_object(app, org, id)?;
            if let Some(upload) = upload {
                store.abort_direct(&key, &upload).await?;
            }
            store.delete(&key).await?;
        } else {
            let store = parts_store(app, org, id)?;
            for sha256 in parts {
                store.delete(&sha256).await?;
            }
        }
        let mut tx = app.store.tx(org).await?;
        sqlx::query("DELETE FROM upload_parts WHERE org_id=$1 AND upload_id=$2")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(upload = %id, error = format!("{error:#}"), "upload not discarded");
    }
}

/// Give up uploads past their expiry, discard what aborted ones left, and forget finished
/// uploads once they expire.
pub async fn collect_org(app: &App, org: Uuid) -> anyhow::Result<()> {
    let mut tx = app.store.tx(org).await?;
    let stale: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE uploads SET state='aborted' WHERE org_id=$1 AND state IN ('open','completing')
         AND expires_at<now() RETURNING id",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    let leftovers: Vec<Uuid> = sqlx::query_scalar(
        "SELECT u.id FROM uploads u WHERE u.org_id=$1 AND u.state='aborted'
         AND EXISTS(SELECT 1 FROM upload_parts p WHERE p.org_id=u.org_id AND p.upload_id=u.id)",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    for id in stale.into_iter().chain(leftovers) {
        discard(app, org, id).await;
    }
    let mut tx = app.store.tx(org).await?;
    sqlx::query(
        "DELETE FROM uploads u WHERE org_id=$1 AND state IN ('completed','aborted') AND expires_at<now()
         AND NOT EXISTS(SELECT 1 FROM upload_parts p WHERE p.org_id=u.org_id AND p.upload_id=u.id)",
    )
    .bind(org)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(size: i64) -> Row {
        Row {
            name: "f".into(),
            size,
            part_size: 10,
            direct: false,
            s3_upload_id: None,
            state: "open".into(),
            attachment_id: None,
        }
    }

    #[test]
    fn parts_cover_the_file_exactly() {
        assert_eq!(part_count(&row(25)), 3);
        assert_eq!(part_length(&row(25), 1), Some(10));
        assert_eq!(part_length(&row(25), 3), Some(5));
        assert_eq!(part_length(&row(25), 4), None);
        assert_eq!(part_length(&row(25), 0), None);
        assert_eq!(part_count(&row(20)), 2);
        assert_eq!(part_length(&row(20), 2), Some(10));
    }
}
