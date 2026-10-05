//! Row types shared across modules, and the column lists that load them.
use anyhow::Context;
use chrono::{DateTime, Utc};
use ryzhub_proto::{Item, ItemDetail, PendingAction};
use serde::Serialize;
use sqlx::types::Json;
use uuid::Uuid;

use super::Tx;

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct Conversation {
    pub id: Uuid,
    pub org_id: Uuid,
    pub creator_id: Uuid,
    pub mode: String,
    pub title: String,
    pub state: String,
    pub project_id: Option<Uuid>,
    pub run_config: serde_json::Value,
    pub temporary: bool,
}

#[derive(sqlx::FromRow)]
pub struct ItemRow {
    pub id: Uuid,
    pub revision: i64,
    pub kind: String,
    pub state: String,
    pub text: String,
    pub first_seq: i64,
    pub last_seq: i64,
    pub run_id: Option<Uuid>,
    pub truncated: bool,
    pub detail: Option<Json<ItemDetail>>,
}

impl From<ItemRow> for Item {
    fn from(row: ItemRow) -> Self {
        Self {
            id: row.id.to_string(),
            revision: row.revision as u64,
            kind: row.kind,
            state: row.state,
            text: row.text,
            first_seq: row.first_seq as u64,
            last_seq: row.last_seq as u64,
            run_id: row.run_id.map(|id| id.to_string()).unwrap_or_default(),
            truncated: row.truncated,
            detail: row.detail.map(|Json(detail)| detail),
        }
    }
}

#[derive(sqlx::FromRow)]
pub struct ActionRow {
    pub id: Uuid,
    pub run_id: Uuid,
    pub run_epoch: i64,
    pub kind: String,
    pub summary: String,
    pub options: Vec<String>,
    pub status: String,
    pub resolved_device: String,
    pub expires_at: DateTime<Utc>,
}

impl From<ActionRow> for PendingAction {
    fn from(row: ActionRow) -> Self {
        Self {
            id: row.id.to_string(),
            run_id: row.run_id.to_string(),
            run_epoch: row.run_epoch as u64,
            summary: row.summary,
            options: row.options,
            status: row.status,
            resolved_device: row.resolved_device,
            expires_at_ms: row.expires_at.timestamp_millis(),
            kind: row.kind,
        }
    }
}

/// The current projection of one timeline item.
pub async fn load_item(tx: &mut Tx<'_>, org: Uuid, id: Uuid) -> anyhow::Result<Item> {
    let row: ItemRow = sqlx::query_as(concat!(
        "SELECT ",
        item_columns!(),
        " FROM items WHERE org_id=$1 AND id=$2"
    ))
    .bind(org)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .with_context(|| format!("item {id} missing"))?;
    Ok(row.into())
}
