//! Knowledge files from documents. An uploaded PDF or DOCX is kept as it came, in the
//! organization's file store next to library files (sharing their content addresses), and its
//! text becomes the knowledge file models read.
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
use uuid::Uuid;

use super::{MAX_PROJECT_BYTES, allowed, owned, valid_name};
use crate::{
    app::App,
    attachments::{content_lock, ingest::Spool},
    error::{ApiResult, Code},
    extract::{self, Format},
    http::member_tx,
    workspace::attachment_stores,
};

#[derive(Deserialize)]
pub struct Upload {
    name: String,
}

/// `POST /orgs/{org}/projects/{project}/files/upload?name=`: add or replace a knowledge file
/// from a document (PDF, DOCX or UTF-8 text) sent as the body.
pub async fn upload(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
    Query(query): Query<Upload>,
    body: Body,
) -> ApiResult<Json<Value>> {
    require!(valid_name(&query.name), Code::InvalidProjectFile);
    // Authorized up front: a slow upload holds no database connection.
    let user = {
        let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
        allowed(&app, &mut tx, org).await?;
        owned(&mut tx, org, ctx.user_id, id, true).await?;
        tx.commit().await?;
        ctx.user_id
    };
    let spool = Spool::receive(body, extract::MAX_SOURCE)
        .await
        .map_err(|error| match error.downcast_ref::<Code>() {
            Some(Code::AttachmentTooLarge) => Code::ProjectFileLimit.into(),
            _ => error,
        })?;
    let (format, media_type) =
        Format::of(spool.head(), &query.name).context(Code::UnsupportedProjectFile)?;
    let program = crate::attachments::renderer(&app.config.attachment_renderer)?;
    let extracted = extract::extract(&program, format, spool.path()).await?;
    require!(!extracted.text.trim().is_empty(), Code::DocumentUnreadable);
    // Text is its own source; a document is kept as it came.
    let source = (format != Format::Text).then_some(&spool);
    let mut tx = app.store.tx(org).await?;
    let policy = allowed(&app, &mut tx, org).await?;
    owned(&mut tx, org, user, id, true).await?;
    let bytes: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(octet_length(content)),0)::bigint FROM project_files
         WHERE org_id=$1 AND project_id=$2 AND name<>$3",
    )
    .bind(org)
    .bind(id)
    .bind(&query.name)
    .fetch_one(&mut *tx)
    .await?;
    let limit = policy
        .project_file_bytes
        .unwrap_or(MAX_PROJECT_BYTES)
        .min(MAX_PROJECT_BYTES);
    require!(
        bytes as u64 + extracted.text.len() as u64 <= limit,
        Code::ProjectFileLimit
    );
    let replaced: Option<String> = sqlx::query_scalar(
        "SELECT source_sha256 FROM project_files WHERE org_id=$1 AND project_id=$2 AND name=$3",
    )
    .bind(org)
    .bind(id)
    .bind(&query.name)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    if let Some(source) = source {
        // Held until commit, so reclamation never deletes the object between storing it and
        // the row that holds it becoming visible.
        content_lock(&mut tx, org, &source.sha256).await?;
    }
    let file: Value = sqlx::query_scalar(
        "INSERT INTO project_files(id,org_id,project_id,name,content,source_sha256,source_type,source_size,truncated)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)
         ON CONFLICT(project_id,name) DO UPDATE SET content=EXCLUDED.content,
           source_sha256=EXCLUDED.source_sha256,source_type=EXCLUDED.source_type,
           source_size=EXCLUDED.source_size,truncated=EXCLUDED.truncated
         RETURNING jsonb_build_object('id',id,'name',name,'source_type',source_type,
           'source_size',source_size,'truncated',truncated,'characters',char_length(content))",
    )
    .bind(Uuid::now_v7())
    .bind(org)
    .bind(id)
    .bind(&query.name)
    .bind(&extracted.text)
    .bind(source.map(|s| &s.sha256))
    .bind(source.map(|_| media_type))
    .bind(source.map(|s| s.size as i64))
    .bind(extracted.truncated)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(source) = source {
        let (primary, secondary) = attachment_stores(&app, org)?;
        for store in std::iter::once(primary).chain(secondary) {
            let reader = tokio::fs::File::open(source.path()).await?;
            store
                .put_stream(&source.sha256, reader, source.size)
                .await?;
        }
    }
    tx.commit().await?;
    if let Some(old) = replaced.filter(|old| Some(old) != source.map(|s| &s.sha256)) {
        release(&app, org, &old).await;
    }
    Ok(Json(
        json!({"saved":true,"effective":"next_turn","file":file}),
    ))
}

/// `GET /orgs/{org}/projects/{project}/files/{file}/source`: the document a knowledge file
/// was read from.
pub async fn source(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id, file)): Path<(Uuid, Uuid, Uuid)>,
) -> ApiResult<Response> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    owned(&mut tx, org, ctx.user_id, id, false).await?;
    let row: Option<(String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT name,source_sha256,source_size FROM project_files
         WHERE org_id=$1 AND project_id=$2 AND id=$3",
    )
    .bind(org)
    .bind(id)
    .bind(file)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let (name, Some(sha256), Some(size)) = row.context(Code::NoDocument)? else {
        return Err(Code::NoDocument.into());
    };
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
            crate::attachments::http::encode_filename(&name)
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

/// Delete the stored document `sha256` unless a library file or another knowledge file still
/// holds it. Called after the row that held it is gone; a failure only leaves the object
/// behind, and is logged.
pub(crate) async fn release(app: &App, org: Uuid, sha256: &str) {
    let result = async {
        let mut tx = app.store.tx(org).await?;
        content_lock(&mut tx, org, sha256).await?;
        let used: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM attachments WHERE org_id=$1 AND sha256=$2
                           AND state IN ('uploading','ready'))
             OR EXISTS(SELECT 1 FROM project_files WHERE org_id=$1 AND source_sha256=$2)",
        )
        .bind(org)
        .bind(sha256)
        .fetch_one(&mut *tx)
        .await?;
        if !used {
            let (primary, secondary) = attachment_stores(app, org)?;
            primary.delete(sha256).await?;
            if let Some(secondary) = secondary {
                secondary.delete(sha256).await?;
            }
        }
        tx.commit().await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(%org, sha256, error = format!("{error:#}"), "project document not released");
    }
}
