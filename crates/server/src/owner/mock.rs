//! The simulator engine: streams a canned reply that echoes the user's message.
use std::time::Duration;
use uuid::Uuid;

use super::{LiveRef, OwnerLink, WorkerEvent};
use crate::{
    app::App,
    runs,
    store::{Lease, load_item},
};

const CHARS_PER_CHUNK: usize = 4;

pub async fn drive(app: App, lease: Lease, run: Uuid, link: OwnerLink) -> anyhow::Result<()> {
    let (item, partial, prompt) = {
        let mut tx = crate::runs::guarded(&app.store, &lease, run).await?;
        let (item, partial): (Uuid, String) = sqlx::query_as(
            "SELECT assistant_item_id,response_text FROM runs WHERE org_id=$1 AND id=$2",
        )
        .bind(lease.org_id)
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
        let prompt: String = sqlx::query_scalar(
            "SELECT text FROM pending_inputs WHERE org_id=$1 AND run_id=$2 AND kind='message' ORDER BY id LIMIT 1",
        )
        .bind(lease.org_id)
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
        let item = load_item(&mut tx, lease.org_id, item).await?;
        tx.commit().await?;
        (item, partial, prompt)
    };
    let reply = format!("模拟 agent 已收到：{prompt}\n\n这是可恢复的流式响应。你好，Rust 🦀。");
    let live = LiveRef::of(&item);
    // A takeover resumes after the text a previous Owner already saved.
    let mut offset = if reply.starts_with(&partial) {
        partial.len()
    } else {
        0
    };
    if offset > 0 {
        link.send(WorkerEvent::Append {
            item: live.clone(),
            stream: crate::owner::TEXT,
            offset: 0,
            text: reply[..offset].into(),
        })
        .await?;
    }
    let mut ticker = tokio::time::interval(Duration::from_millis(app.config.mock_chunk_ms));
    while offset < reply.len() {
        ticker.tick().await;
        let end = reply[offset..]
            .char_indices()
            .nth(CHARS_PER_CHUNK)
            .map_or(reply.len(), |(index, _)| offset + index);
        link.send(WorkerEvent::Append {
            item: live.clone(),
            stream: crate::owner::TEXT,
            offset,
            text: reply[offset..end].into(),
        })
        .await?;
        offset = end;
    }
    let mut tx = crate::runs::guarded(&app.store, &lease, run).await?;
    sqlx::query("UPDATE runs SET response_text=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(run)
        .bind(&reply)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE pending_inputs SET consumed_step=0 WHERE org_id=$1 AND run_id=$2 AND consumed_step IS NULL")
        .bind(lease.org_id)
        .bind(run)
        .execute(&mut *tx)
        .await?;
    runs::finish(&mut tx, &lease, run, "completed", None).await?;
    tx.commit().await?;
    link.committed().await;
    Ok(())
}
