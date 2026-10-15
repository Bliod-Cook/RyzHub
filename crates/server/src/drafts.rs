//! Drafts: each conversation's unsent message, synced across its creator's devices. A write
//! replaces the draft (the latest wins) and reaches the user's other devices as `DraftChanged`
//! on their user channel; the channel's snapshot carries every draft not cleared.
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use ryzhub_domain::OrgContext;
use ryzhub_proto::{Draft, event_envelope::Event};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::App,
    error::{ApiResult, Code},
    store::{Tx, append, conversation_in},
};

/// Longest draft, as for a message.
const MAX_TEXT: usize = 65_536;

#[derive(Deserialize)]
pub struct DraftInput {
    #[serde(default)]
    text: String,
    /// The device writing it, so it can tell its own writes from others'.
    #[serde(default)]
    device_id: String,
}

/// `PUT /orgs/{org}/conversations/{conv}/draft`: replaces the draft; empty clears it.
pub async fn put(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Json(input): Json<DraftInput>,
) -> ApiResult<Json<Value>> {
    require!(
        input.text.len() <= MAX_TEXT && input.device_id.len() <= 128,
        Code::InvalidInput
    );
    let (ctx, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    conversation_in(&mut tx, &ctx, conv).await?;
    let updated_at_ms: i64 = sqlx::query_scalar(
        "INSERT INTO drafts(org_id,user_id,conversation_id,text,device_id)
         VALUES($1,$2,$3,$4,$5)
         ON CONFLICT(user_id,conversation_id) DO UPDATE SET text=excluded.text,
           device_id=excluded.device_id,updated_at=now()
         RETURNING (extract(epoch FROM updated_at)*1000)::bigint",
    )
    .bind(org)
    .bind(ctx.user_id)
    .bind(conv)
    .bind(&input.text)
    .bind(&input.device_id)
    .fetch_one(&mut *tx)
    .await?;
    append(
        &mut tx,
        org,
        &format!("user:{org}:{}", ctx.user_id),
        None,
        "",
        Event::DraftChanged(Draft {
            conversation_id: conv.to_string(),
            text: input.text,
            updated_at_ms,
            device_id: input.device_id,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"updated_at_ms": updated_at_ms})))
}

/// The user's drafts not cleared, for their user channel's snapshot.
pub async fn of_user(tx: &mut Tx<'_>, ctx: &OrgContext) -> anyhow::Result<Vec<Draft>> {
    let rows: Vec<(Uuid, String, i64, String)> = sqlx::query_as(
        "SELECT d.conversation_id,d.text,(extract(epoch FROM d.updated_at)*1000)::bigint,d.device_id
         FROM drafts d JOIN conversations c ON c.org_id=d.org_id AND c.id=d.conversation_id
         WHERE d.org_id=$1 AND d.user_id=$2 AND c.state IN ('active','archived') AND d.text<>''",
    )
    .bind(ctx.org_id)
    .bind(ctx.user_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(conv, text, updated_at_ms, device_id)| Draft {
            conversation_id: conv.to_string(),
            text,
            updated_at_ms,
            device_id,
        })
        .collect())
}
