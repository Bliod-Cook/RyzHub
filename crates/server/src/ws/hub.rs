//! Per-process channel fan-out.
//!
//! Each channel has at most one [`Feed`] per gateway process: one NATS subscription whose
//! frames are broadcast to every local subscriber. The feed drops frames from retired
//! conversation publishers (writer epoch below the current one, learned from the database and
//! from channel-writer notices) and repairs a lost tail message with a cheap periodic head check.
use futures_util::StreamExt;
use prost::Message;
use ryzhub_proto::{Frame, frame::Body};
use std::{
    collections::HashMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex, broadcast},
    task::JoinHandle,
};
use uuid::Uuid;

use crate::{owner::subject, revocation::Revocations, store::Store};

/// Frames a slow subscriber may fall behind before it is told to resubscribe.
const FEED_BUFFER: usize = 1024;
/// A head beyond the last seen event for this long is treated as a lost NATS message.
const AUDIT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Hub {
    inner: Arc<HubInner>,
}

struct HubInner {
    store: Store,
    nats: async_nats::Client,
    revocations: Revocations,
    feeds: Mutex<HashMap<String, Weak<Feed>>>,
}

pub struct Feed {
    sender: broadcast::Sender<Arc<Frame>>,
    epoch: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl Feed {
    /// Frames published after this call. Subscribe before reading the replay snapshot.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Frame>> {
        self.sender.subscribe()
    }

    /// Current writer epoch of a conversation channel (0 for user/org channels).
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Hub {
    pub fn new(store: Store, nats: async_nats::Client, revocations: Revocations) -> Self {
        Self {
            inner: Arc::new(HubInner {
                store,
                nats,
                revocations,
                feeds: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The live feed for `channel`, created on first use. The NATS subscription is flushed
    /// before this returns, so nothing published afterwards can be missed.
    pub async fn feed(&self, org: Uuid, channel: &str) -> anyhow::Result<Arc<Feed>> {
        let mut feeds = self.inner.feeds.lock().await;
        feeds.retain(|_, feed| feed.strong_count() > 0);
        if let Some(feed) = feeds.get(channel).and_then(Weak::upgrade) {
            return Ok(feed);
        }
        let messages = self.inner.nats.subscribe(subject(channel)).await?;
        self.inner.nats.flush().await?;
        let (head, epoch) = channel_state(&self.inner.store, org, channel).await?;
        let (sender, _) = broadcast::channel(FEED_BUFFER);
        let epoch = Arc::new(AtomicU64::new(epoch));
        let task = tokio::spawn(run_feed(FeedTask {
            store: self.inner.store.clone(),
            org,
            channel: channel.to_owned(),
            sender: sender.clone(),
            epoch: epoch.clone(),
            last_seq: head,
            notices: self.inner.revocations.subscribe(),
            messages,
        }));
        let feed = Arc::new(Feed {
            sender,
            epoch,
            task,
        });
        feeds.insert(channel.to_owned(), Arc::downgrade(&feed));
        Ok(feed)
    }
}

struct FeedTask {
    store: Store,
    org: Uuid,
    channel: String,
    sender: broadcast::Sender<Arc<Frame>>,
    epoch: Arc<AtomicU64>,
    /// Highest durable seq broadcast so far.
    last_seq: u64,
    notices: broadcast::Receiver<crate::revocation::Notice>,
    messages: async_nats::Subscriber,
}

async fn run_feed(mut feed: FeedTask) {
    let conversation = feed.channel.starts_with("conv:");
    let mut audit = tokio::time::interval(AUDIT_INTERVAL);
    audit.tick().await;
    // The tail position observed at the previous audit while behind the database head.
    let mut stalled_at: Option<u64> = None;
    loop {
        tokio::select! {
            message = feed.messages.next() => {
                let Some(message) = message else { return };
                let Ok(frame) = Frame::decode(message.payload) else { continue };
                if conversation && !feed.accept_epoch(frame.writer_epoch) {
                    continue;
                }
                if let Some(Body::Event(event)) = &frame.body {
                    feed.last_seq = feed.last_seq.max(event.seq);
                }
                let _ = feed.sender.send(Arc::new(frame));
            }
            notice = feed.notices.recv() => match notice {
                Ok(notice) if notice.channel.as_deref() == Some(feed.channel.as_str()) => {
                    if let Some(epoch) = notice.writer_epoch {
                        feed.epoch.fetch_max(epoch as u64, Ordering::AcqRel);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
                _ => {}
            },
            _ = audit.tick() => {
                if let Err(error) = feed.audit(&mut stalled_at).await {
                    tracing::debug!(channel = feed.channel, %error, "feed audit deferred");
                }
            }
        }
    }
}

impl FeedTask {
    /// Frames from a retired writer are dropped; a newer epoch retires the current one.
    fn accept_epoch(&self, epoch: u64) -> bool {
        let current = self.epoch.fetch_max(epoch, Ordering::AcqRel);
        epoch >= current
    }

    async fn audit(&mut self, stalled_at: &mut Option<u64>) -> anyhow::Result<()> {
        let (head, epoch) = channel_state(&self.store, self.org, &self.channel).await?;
        self.epoch.fetch_max(epoch, Ordering::AcqRel);
        if head <= self.last_seq {
            *stalled_at = None;
            return Ok(());
        }
        // Allow one interval for in-flight publication before repairing from the log.
        if *stalled_at != Some(self.last_seq) {
            *stalled_at = Some(self.last_seq);
            return Ok(());
        }
        let mut tx = self.store.tx(self.org).await?;
        let frames =
            crate::store::events_between(&mut tx, self.org, &self.channel, self.last_seq, head)
                .await?;
        tx.commit().await?;
        for mut frame in frames {
            frame.writer_epoch = self.epoch.load(Ordering::Acquire);
            let _ = self.sender.send(Arc::new(frame));
        }
        self.last_seq = head;
        *stalled_at = None;
        Ok(())
    }
}

async fn channel_state(store: &Store, org: Uuid, channel: &str) -> anyhow::Result<(u64, u64)> {
    let mut tx = store.tx(org).await?;
    let (head, epoch): (i64, Option<i64>) =
        sqlx::query_as("SELECT last_seq,writer_epoch FROM channels WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(channel)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok((head as u64, epoch.unwrap_or(0) as u64))
}
