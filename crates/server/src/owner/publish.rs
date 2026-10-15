//! Publication of committed events and ephemeral stream frames.
use anyhow::Context;
use prost::Message;
use ryzhub_proto::{EventEnvelope, Frame, frame::Body};
use std::time::Duration;
use uuid::Uuid;

use super::subject;
use crate::{
    error::Code,
    store::{Lease, Store},
};

/// Frames above this are not sent live; subscribers repair the resulting seq gap from the
/// event log. Most items are far smaller, but a bounded item can still exceed it (256 file
/// changes of 64 KiB each).
fn fits(nats: &async_nats::Client, frame: &[u8]) -> bool {
    frame.len() + 1024 <= nats.server_info().max_payload
}

async fn send(nats: &async_nats::Client, channel: &str, frame: Frame) -> anyhow::Result<()> {
    let bytes = frame.encode_to_vec();
    if !fits(nats, &bytes) {
        tracing::warn!(
            channel,
            bytes = bytes.len(),
            "frame exceeds NATS payload; left to replay"
        );
        return Ok(());
    }
    nats.publish(subject(channel), bytes.into()).await?;
    Ok(())
}

/// Publish every committed-but-unpublished event, then `transient` frames, under the channel
/// row lock, and advance `published_seq`. Holding the lock across the flush keeps a new Owner
/// from interleaving with a retired one; a crash after publishing only causes duplicates,
/// which subscribers drop by seq.
pub async fn publish(
    store: &Store,
    nats: &async_nats::Client,
    org: Uuid,
    channel: &str,
    lease: Option<&Lease>,
    transient: Vec<Frame>,
) -> anyhow::Result<()> {
    let mut tx = store.tx(org).await?;
    let row = sqlx::query!(
        "SELECT last_seq,published_seq FROM channels WHERE org_id=$1 AND id=$2
         AND (($3::bigint IS NULL AND writer_epoch IS NULL)
              OR (writer_epoch=$3 AND owner_id=$4 AND lease_until>clock_timestamp())) FOR UPDATE",
        org,
        channel,
        lease.map(|l| l.epoch),
        lease.map(|l| l.owner_id)
    )
    .fetch_optional(&mut *tx)
    .await?
    .context(Code::Fenced)?;
    let (head, published) = (row.last_seq, row.published_seq);
    if head == published && transient.is_empty() {
        tx.commit().await?;
        return Ok(());
    }
    let epoch = lease.map_or(0, |l| l.epoch as u64);
    let payloads: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT payload FROM events WHERE org_id=$1 AND channel_id=$2 AND seq>$3 ORDER BY seq",
    )
    .bind(org)
    .bind(channel)
    .bind(published)
    .fetch_all(&mut *tx)
    .await?;
    for payload in payloads {
        let mut frame = ryzhub_proto::frame(
            channel,
            Body::Event(EventEnvelope::decode(payload.as_slice())?),
        );
        frame.writer_epoch = epoch;
        send(nats, channel, frame).await?;
    }
    for mut frame in transient {
        frame.writer_epoch = epoch;
        send(nats, channel, frame).await?;
    }
    tokio::time::timeout(Duration::from_millis(750), nats.flush()).await??;
    if head > published {
        sqlx::query("UPDATE channels SET published_seq=$3 WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(channel)
            .bind(head)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Stream frames with no pending durable events: sent straight to NATS. Each carries the
/// writer epoch, so gateways drop frames from a publisher that has since been retired.
pub async fn publish_transient(
    nats: &async_nats::Client,
    lease: &Lease,
    frames: Vec<Frame>,
) -> anyhow::Result<()> {
    let channel = lease.channel();
    for mut frame in frames {
        frame.writer_epoch = lease.epoch as u64;
        send(nats, &channel, frame).await?;
    }
    Ok(())
}
