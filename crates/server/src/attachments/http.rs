//! `/orgs/{org}/attachments`: the caller's own file library. Only the uploader reaches a file.
use anyhow::Context;
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::{
    DEFAULT_BYTES, Kind, MAX_BYTES, display_name,
    ingest::{Spool, ingest},
};
use crate::{
    app::App,
    error::{ApiResult, Code},
    http::member_tx,
    store::{Tx, effective_policy},
    workspace::attachment_stores,
};

/// Uploads received at once; more are turned away rather than queued.
static UPLOADS: Semaphore = Semaphore::const_new(8);
/// Library page size.
const PAGE: i64 = 50;

#[derive(Deserialize)]
pub struct Upload {
    name: String,
}

/// `POST /orgs/{org}/attachments?name=`: add the request body to the caller's library. The
/// type comes from the content; images get a rendition models can read.
pub async fn upload(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Query(query): Query<Upload>,
    body: Body,
) -> ApiResult<Json<Value>> {
    let name = display_name(&query.name).context(Code::InvalidAttachmentName)?;
    // Authorized up front: a slow upload holds no database connection.
    let (ctx, policy) = {
        let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
        let policy = effective_policy(&mut tx, &app.config.policy, org).await?;
        tx.commit().await?;
        (ctx, policy)
    };
    let limit = policy
        .attachment_bytes
        .unwrap_or(DEFAULT_BYTES)
        .min(MAX_BYTES);
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
    require!(
        declared.is_none_or(|size| size <= limit),
        Code::AttachmentTooLarge
    );
    let _permit = UPLOADS.try_acquire().map_err(|_| Code::UploadBusy)?;
    let spool = Spool::receive(body, limit).await?;
    let id = ingest(
        &app,
        org,
        ctx.user_id,
        &name,
        "upload",
        &spool,
        policy.attachment_quota_bytes,
    )
    .await?;
    let mut tx = app.store.tx(org).await?;
    let file = describe(&mut tx, org, ctx.user_id, id).await?;
    tx.commit().await?;
    Ok(Json(file))
}

/// What the library shows for a file: no content. A macro, so it composes into static SQL.
macro_rules! file_fields {
    () => {
        "jsonb_build_object('id',a.id,'name',a.name,'media_type',a.media_type,'direction',a.direction,
         'kind',a.kind,'size',a.size,'sha256',a.sha256,'width',a.width,'height',a.height,
         'created_at',a.created_at,
         'preview',a.model_data IS NOT NULL,
         'conversations',(SELECT count(*) FROM conversation_files f JOIN conversations c
             ON c.org_id=f.org_id AND c.id=f.conversation_id
             WHERE f.org_id=a.org_id AND f.attachment_id=a.id AND c.state<>'deleted'))"
    };
}

async fn describe(tx: &mut Tx<'_>, org: Uuid, user: Uuid, id: Uuid) -> anyhow::Result<Value> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT ",
        file_fields!(),
        " FROM attachments a WHERE a.org_id=$1 AND a.user_id=$2 AND a.id=$3"
    ))
    .bind(org)
    .bind(user)
    .bind(id)
    .fetch_one(&mut **tx)
    .await?)
}

#[derive(Deserialize)]
pub struct Page {
    before: Option<Uuid>,
    limit: Option<i64>,
    /// Only files whose name contains this, ignoring case.
    q: Option<String>,
    /// Only files of these kinds, separated by commas.
    kind: Option<String>,
    /// Only files the member uploaded (`upload`) or models published (`output`).
    direction: Option<String>,
    /// Only files with this content: its SHA-256 in hex.
    sha256: Option<String>,
}

/// Longest search accepted, in characters.
const QUERY_CHARS: usize = 200;

/// `GET /orgs/{org}/attachments?before=&limit=&q=&kind=&direction=&sha256=`: the caller's files,
/// newest first.
pub async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult<Json<Value>> {
    let query = page.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    if query.is_some_and(|q| q.chars().count() > QUERY_CHARS) {
        return Err(Code::InvalidInput.into());
    }
    let kinds: Option<Vec<&str>> = page.kind.as_deref().map(|kinds| kinds.split(',').collect());
    if kinds
        .iter()
        .flatten()
        .any(|kind| Kind::parse(kind).is_none())
    {
        return Err(Code::InvalidInput.into());
    }
    if page
        .direction
        .as_deref()
        .is_some_and(|direction| !matches!(direction, "upload" | "output"))
    {
        return Err(Code::InvalidInput.into());
    }
    let sha256 = page.sha256.as_deref().map(|hex| hex.trim().to_ascii_lowercase());
    if sha256
        .as_deref()
        .is_some_and(|hex| hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(Code::InvalidInput.into());
    }
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let files: Vec<Value> = sqlx::query_scalar(concat!(
        "SELECT ",
        file_fields!(),
        " FROM attachments a
         WHERE a.org_id=$1 AND a.user_id=$2 AND a.state='ready' AND ($3::uuid IS NULL OR a.id<$3)
           AND ($5::text IS NULL OR strpos(lower(a.name),lower($5))>0)
           AND ($6::text[] IS NULL OR a.kind=ANY($6))
           AND ($7::text IS NULL OR a.direction=$7)
           AND ($8::text IS NULL OR a.sha256=$8)
         ORDER BY a.id DESC LIMIT $4"
    ))
    .bind(org)
    .bind(ctx.user_id)
    .bind(page.before)
    .bind(page.limit.unwrap_or(PAGE).clamp(1, PAGE))
    .bind(query)
    .bind(kinds)
    .bind(page.direction.as_deref())
    .bind(sha256)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"files": files})))
}

/// The caller's file `id`: its state, content address and size.
async fn owned(
    tx: &mut Tx<'_>,
    org: Uuid,
    user: Uuid,
    id: Uuid,
) -> anyhow::Result<(String, String, i64, String)> {
    let row: Option<(String, String, i64, String)> = sqlx::query_as(
        "SELECT state,sha256,size,name FROM attachments WHERE org_id=$1 AND user_id=$2 AND id=$3",
    )
    .bind(org)
    .bind(user)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    let row = row.context(Code::AttachmentUnavailable)?;
    match row.0.as_str() {
        "ready" => Ok(row),
        "uploading" => Err(Code::AttachmentUnavailable.into()),
        _ => Err(Code::AttachmentDeleted.into()),
    }
}

/// `GET /orgs/{org}/attachments/{id}/content`: the file as uploaded, always as a download.
pub async fn content(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let (_, sha256, size, name) = owned(&mut tx, org, ctx.user_id, id).await?;
    tx.commit().await?;
    let (primary, secondary) = attachment_stores(&app, org)?;
    let reader = match primary.open(&sha256).await {
        Ok((_, reader)) => reader,
        Err(error) => match &secondary {
            Some(secondary) => secondary.open(&sha256).await?.1,
            None => return Err(error.into()),
        },
    };
    let mut response = Body::from_stream(tokio_util::io::ReaderStream::new(reader)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, size.into());
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename*=UTF-8''{}",
            encode_filename(&name)
        ))?,
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

/// `GET /orgs/{org}/attachments/{id}/preview`: an image's rendition, in the type the server
/// produced it in.
pub async fn preview(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    owned(&mut tx, org, ctx.user_id, id).await?;
    let (media_type, data): (Option<String>, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT model_media_type,model_data FROM attachments WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let (Some(media_type), Some(data)) = (media_type, data) else {
        return Err(Code::AttachmentUnavailable.into());
    };
    let mut response = data.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&media_type)?);
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    Ok(response)
}

/// `DELETE /orgs/{org}/attachments/{id}`: delete the file everywhere. Conversations that
/// referred to it lose the reference; models and sandboxes stop seeing it from their next
/// request. The content is reclaimed in the background.
pub async fn remove(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM attachments WHERE org_id=$1 AND user_id=$2 AND id=$3 FOR UPDATE",
    )
    .bind(org)
    .bind(ctx.user_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let state = state.context(Code::AttachmentUnavailable)?;
    if matches!(state.as_str(), "ready" | "uploading") {
        delete(&mut tx, org, id).await?;
    }
    tx.commit().await?;
    Ok(Json(json!({"deleted": true})))
}

/// Mark a file deleted and drop every reference to it.
pub(crate) async fn delete(tx: &mut Tx<'_>, org: Uuid, id: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE attachments SET state='deleted',deleted_at=now(),model_data=NULL,text_content=NULL
         WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM conversation_files WHERE org_id=$1 AND attachment_id=$2")
        .bind(org)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// RFC 8187 `filename*` value.
fn encode_filename(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn download_names_are_percent_encoded() {
        assert_eq!(
            super::encode_filename("a b;\"图.png"),
            "a%20b%3B%22%E5%9B%BE.png"
        );
    }
}
