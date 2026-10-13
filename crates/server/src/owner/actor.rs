//! One conversation's Owner.
//!
//! The actor holds the channel lease and reacts to four inputs: wake-ups (queued commands),
//! worker events (stream text, committed events), worker exits and a one-second housekeeping
//! tick. Stream text is coalesced in memory: the first token of an item is sent at once, later
//! text in [`FLUSH`] batches. Durable events are published before the stream frames that
//! follow them.
use futures_util::StreamExt;
use prost::Message;
use ryzhub_proto::{Delta, Frame, FrameBatch, Item, ItemFold, frame, frame::Body};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc},
    task::{AbortHandle, JoinHandle, JoinSet},
    time::{Instant, MissedTickBehavior},
};
use uuid::Uuid;

use super::{Engine, fold_subject, mock, process_next, publish, publish_transient, result_subject};
use crate::{
    app::App,
    error::{is_fenced, public_code},
    runs,
    store::Lease,
};

/// Stream text coalescing interval.
const FLUSH: Duration = Duration::from_millis(40);
const HOUSEKEEPING: Duration = Duration::from_secs(1);
/// An actor with nothing to do releases the conversation after this long.
const IDLE_EXIT: Duration = Duration::from_secs(5);
/// Commands processed per wake before other inputs get a turn.
const COMMAND_BATCH: usize = 32;

/// Identity of a streaming item revision. Stream frames apply only on top of this revision.
#[derive(Clone, Debug)]
pub struct LiveRef {
    pub id: String,
    pub run_id: String,
    pub revision: u64,
    pub base_seq: u64,
}

impl LiveRef {
    /// `item` as returned by a commit, so `last_seq` is its event's seq.
    pub fn of(item: &Item) -> Self {
        Self {
            id: item.id.clone(),
            run_id: item.run_id.clone(),
            revision: item.revision,
            base_seq: item.last_seq,
        }
    }
}

/// The live stream of a reply's text.
pub const TEXT: &str = "text";
/// The live stream of a command's output.
pub const TERMINAL_OUTPUT: &str = "terminal_output";

pub enum WorkerEvent {
    /// Text appended at byte `offset` of a streaming item's `stream`.
    Append {
        item: LiveRef,
        stream: &'static str,
        offset: usize,
        text: String,
    },
    /// The complete in-progress text of an item's `stream` (e.g. a harness fold after
    /// reconnecting).
    Replace {
        item: LiveRef,
        stream: &'static str,
        text: String,
    },
    /// The worker committed durable events.
    Committed,
}

/// A worker's connection to its Owner.
#[derive(Clone)]
pub struct OwnerLink {
    events: mpsc::Sender<WorkerEvent>,
    wake: Arc<Notify>,
}

impl OwnerLink {
    /// A link with no Owner behind it, for tools and tests: stream events are discarded.
    pub fn detached() -> Self {
        let (events, mut discard) = mpsc::channel(64);
        tokio::spawn(async move { while discard.recv().await.is_some() {} });
        Self {
            events,
            wake: Arc::new(Notify::new()),
        }
    }

    /// A stream event. Fails with FENCED once the Owner is gone.
    pub async fn send(&self, event: WorkerEvent) -> anyhow::Result<()> {
        self.events
            .send(event)
            .await
            .map_err(|_| crate::error::Code::Fenced.into())
    }

    pub async fn committed(&self) {
        let _ = self.events.send(WorkerEvent::Committed).await;
    }

    /// Resolves after the Owner processed new commands (input, answers, cancellation).
    pub async fn commands_processed(&self) {
        self.wake.notified().await;
    }
}

struct Live {
    item: LiveRef,
    stream: &'static str,
    text: String,
    /// Bytes already sent as Delta frames.
    published: usize,
    /// Bytes already written to `runs.response_text`.
    persisted: usize,
    /// The text was replaced by something that does not extend what clients have.
    needs_fold: bool,
}

impl Live {
    fn new(item: LiveRef, stream: &'static str) -> Self {
        Self {
            item,
            stream,
            text: String::new(),
            published: 0,
            persisted: 0,
            needs_fold: false,
        }
    }

    fn fold(&self, channel: &str) -> Frame {
        frame(
            channel,
            Body::ItemFold(ItemFold {
                item_id: self.item.id.clone(),
                stream: self.stream.into(),
                part_index: 0,
                text: self.text.clone(),
                base_seq: self.item.base_seq,
                revision: self.item.revision,
            }),
        )
    }
}

struct Actor {
    app: App,
    lease: Lease,
    engine: Engine,
    link: OwnerLink,
    workers: HashMap<Uuid, AbortHandle>,
    task_runs: HashMap<tokio::task::Id, Uuid>,
    exits: JoinSet<anyhow::Result<()>>,
    /// Live streams by item and stream.
    live: HashMap<(String, &'static str), Live>,
    /// Committed events may be waiting for publication.
    events_pending: bool,
    jobs: Option<JoinHandle<anyhow::Result<()>>>,
    idle_since: Option<Instant>,
}

impl Drop for Actor {
    fn drop(&mut self) {
        if let Some(jobs) = &self.jobs {
            jobs.abort();
        }
    }
}

pub async fn run(app: App, lease: Lease, wake: Arc<Notify>) -> anyhow::Result<()> {
    let mode: String = {
        let mut tx = app.store.tx(lease.org_id).await?;
        let mode = sqlx::query_scalar("SELECT mode FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        mode
    };
    let (events_tx, mut events) = mpsc::channel(256);
    let mut folds = app.nats.subscribe(fold_subject(&lease.channel())).await?;
    let mut renew = tokio::time::interval(Duration::from_millis(app.config.owner_lease_ms / 3));
    let mut housekeeping = tokio::time::interval(HOUSEKEEPING);
    let mut flush = tokio::time::interval(FLUSH);
    for timer in [&mut renew, &mut housekeeping, &mut flush] {
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    }
    let mut actor = Actor {
        engine: Engine::of(&app.config, &mode),
        app,
        lease,
        link: OwnerLink {
            events: events_tx,
            wake: Arc::new(Notify::new()),
        },
        workers: HashMap::new(),
        task_runs: HashMap::new(),
        exits: JoinSet::new(),
        live: HashMap::new(),
        events_pending: true,
        jobs: None,
        idle_since: None,
    };
    loop {
        tokio::select! {
            _ = renew.tick() => {
                actor.app.store.renew(&actor.lease, actor.app.config.owner_lease_ms).await?;
            }
            () = wake.notified() => actor.on_wake().await?,
            _ = housekeeping.tick() => {
                if !actor.housekeep().await? {
                    return Ok(());
                }
            }
            Some(event) = events.recv() => actor.on_event(event).await?,
            Some(exit) = actor.exits.join_next_with_id() => actor.on_exit(exit).await?,
            _ = flush.tick() => actor.flush().await?,
            Some(request) = folds.next() => actor.reply_folds(request).await?,
        }
    }
}

impl Actor {
    /// Process queued commands, let workers observe them, publish their events and then
    /// push each stored result to the gateway waiting for it.
    async fn on_wake(&mut self) -> anyhow::Result<()> {
        // A cancellation below must see the latest partial text.
        self.persist_live().await?;
        let mut results = vec![];
        while results.len() < COMMAND_BATCH {
            match process_next(&self.app.store, &self.lease, &self.app.config).await? {
                Some(processed) => results.push(processed),
                None => break,
            }
        }
        if results.is_empty() {
            return Ok(());
        }
        self.link.wake.notify_waiters();
        self.reconcile_workers().await?;
        self.events_pending = true;
        self.flush().await?;
        for processed in results {
            let subject = result_subject(processed.user, processed.cid);
            self.app
                .nats
                .publish(subject, processed.response.encode_to_vec().into())
                .await?;
        }
        Ok(())
    }

    /// Returns false when the actor should release the conversation.
    async fn housekeep(&mut self) -> anyhow::Result<bool> {
        self.spawn_jobs();
        // Commands whose wake-up was lost (or queued before this actor existed).
        self.on_wake().await?;
        {
            let mut tx = self.app.store.fenced(&self.lease).await?;
            runs::expire_invalid(&mut tx, &self.lease).await?;
            tx.commit().await?;
        }
        let active = self.reconcile_workers().await?;
        self.persist_live().await?;
        self.prune_live().await?;
        // Catch-all publication; a no-op when the channel has nothing unpublished.
        self.events_pending = true;
        self.flush().await?;
        let jobs_running = self.jobs.as_ref().is_some_and(|jobs| !jobs.is_finished());
        if active > 0 || jobs_running || !self.workers.is_empty() || !self.live.is_empty() {
            self.idle_since = None;
            return Ok(true);
        }
        let idle = *self.idle_since.get_or_insert_with(Instant::now);
        Ok(idle.elapsed() < IDLE_EXIT)
    }

    /// Background maintenance for this conversation: fork/compact operations, workspace jobs,
    /// harness interrupts and chat sandbox cleanup. One pass at a time.
    fn spawn_jobs(&mut self) {
        if let Some(finished) = self.jobs.take_if(|jobs| jobs.is_finished())
            && let Some(Ok(Err(error))) = futures_util::FutureExt::now_or_never(finished)
            && !is_fenced(&error)
        {
            tracing::warn!(conv = %self.lease.conversation_id, error = format!("{error:#}"), "conversation jobs failed");
        }
        if self.jobs.is_some() || self.engine == Engine::Mock {
            return;
        }
        let (app, lease, work, link) = (
            self.app.clone(),
            self.lease.clone(),
            self.engine == Engine::Work,
            self.link.clone(),
        );
        self.jobs = Some(tokio::spawn(async move {
            crate::conversation_commands::jobs(&app, &lease, &link).await?;
            if work {
                crate::workspace::jobs(&app, &lease, &link).await?;
                crate::work::interrupt_terminal(&app, &lease).await?;
            }
            crate::sandbox::cleanup(&app, &lease, false).await?;
            link.committed().await;
            Ok(())
        }));
    }

    /// Start a worker for every run that needs one and stop workers whose run ended.
    /// Returns the number of active runs.
    async fn reconcile_workers(&mut self) -> anyhow::Result<usize> {
        let mut tx = self.app.store.tx(self.lease.org_id).await?;
        let runs: Vec<(Uuid, String, bool)> = sqlx::query_as(
            "SELECT id,state,read_only FROM runs WHERE org_id=$1 AND conversation_id=$2
             AND state IN ('running','requires_action')",
        )
        .bind(self.lease.org_id)
        .bind(self.lease.conversation_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        // A Work harness keeps its turn open while it waits for an approval; the Chat loop
        // and the simulator stop and are restarted once the run is running again.
        let engine = self.engine;
        let wanted: HashMap<Uuid, bool> = runs
            .iter()
            .filter(|(_, state, read_only)| {
                state == "running" || (engine == Engine::Work && !read_only)
            })
            .map(|(id, _, read_only)| (*id, *read_only))
            .collect();
        self.workers.retain(|run, handle| {
            let keep = wanted.contains_key(run);
            if !keep {
                handle.abort();
            }
            keep
        });
        for (run, read_only) in wanted {
            if !self.workers.contains_key(&run) {
                self.spawn_worker(run, read_only);
            }
        }
        Ok(runs.len())
    }

    fn spawn_worker(&mut self, run: Uuid, read_only: bool) {
        let (app, lease, link) = (self.app.clone(), self.lease.clone(), self.link.clone());
        let handle = match (self.engine, read_only) {
            (Engine::Mock, _) => self.exits.spawn(mock::drive(app, lease, run, link)),
            (Engine::Work, false) => self.exits.spawn(crate::work::drive(app, lease, run, link)),
            _ => self
                .exits
                .spawn(crate::chat::runner::drive(app, lease, run, link)),
        };
        self.task_runs.insert(handle.id(), run);
        self.workers.insert(run, handle);
    }

    async fn on_exit(
        &mut self,
        exit: Result<(tokio::task::Id, anyhow::Result<()>), tokio::task::JoinError>,
    ) -> anyhow::Result<()> {
        let (id, result) = match exit {
            Ok((id, result)) => (id, result),
            Err(error) if error.is_cancelled() => {
                self.task_runs.remove(&error.id());
                return Ok(());
            }
            Err(error) => (error.id(), Err(anyhow::anyhow!("worker panicked: {error}"))),
        };
        let Some(run) = self.task_runs.remove(&id) else {
            return Ok(());
        };
        self.workers.remove(&run);
        let Err(error) = result else {
            return Ok(());
        };
        if is_fenced(&error) {
            return Ok(());
        }
        tracing::warn!(%run, error = format!("{error:#}"), "run failed");
        let code = public_code(&error);
        let mut tx = self.app.store.fenced(&self.lease).await?;
        runs::finish(&mut tx, &self.lease, run, "failed", Some(code)).await?;
        tx.commit().await?;
        self.events_pending = true;
        Ok(())
    }

    async fn on_event(&mut self, event: WorkerEvent) -> anyhow::Result<()> {
        match event {
            WorkerEvent::Committed => self.events_pending = true,
            WorkerEvent::Append {
                item,
                stream,
                offset,
                text,
            } => {
                let live = self.live_for(item, stream);
                let first = live.published == 0 && live.text.is_empty();
                // Ignore text the actor already has (a restarted worker re-sending a prefix).
                if offset + text.len() > live.text.len() && offset <= live.text.len() {
                    let skip = live.text.len() - offset;
                    if text.is_char_boundary(skip) {
                        live.text.push_str(&text[skip..]);
                    }
                }
                // The first token goes out immediately; later text waits for the flush tick.
                if first {
                    self.flush().await?;
                }
            }
            WorkerEvent::Replace { item, stream, text } => {
                let live = self.live_for(item, stream);
                if !text.starts_with(&live.text[..live.published]) {
                    live.needs_fold = true;
                }
                live.text = text;
                live.published = live.published.min(live.text.len());
            }
        }
        Ok(())
    }

    fn live_for(&mut self, item: LiveRef, stream: &'static str) -> &mut Live {
        let live = self
            .live
            .entry((item.id.clone(), stream))
            .or_insert_with(|| Live::new(item.clone(), stream));
        if live.item.revision != item.revision || live.item.base_seq != item.base_seq {
            *live = Live::new(item, stream);
        }
        live
    }

    /// Publish pending events (under the channel lock) and coalesced stream text.
    async fn flush(&mut self) -> anyhow::Result<()> {
        let channel = self.lease.channel();
        let mut frames = vec![];
        for live in self.live.values_mut() {
            if live.needs_fold {
                frames.push(live.fold(&channel));
                live.needs_fold = false;
                live.published = live.text.len();
            } else if live.published < live.text.len() {
                frames.push(frame(
                    &channel,
                    Body::Delta(Delta {
                        item_id: live.item.id.clone(),
                        stream: live.stream.into(),
                        part_index: 0,
                        offset: live.published as u64,
                        text: live.text[live.published..].into(),
                        base_seq: live.item.base_seq,
                        revision: live.item.revision,
                    }),
                ));
                live.published = live.text.len();
            }
        }
        if self.events_pending {
            self.events_pending = false;
            let lease = &self.lease;
            publish(
                &self.app.store,
                &self.app.nats,
                lease.org_id,
                &channel,
                Some(lease),
                frames,
            )
            .await
        } else if frames.is_empty() {
            Ok(())
        } else {
            publish_transient(&self.app.nats, &self.lease, frames).await
        }
    }

    /// Save partial replies so a cancellation or failure keeps what was already shown.
    async fn persist_live(&mut self) -> anyhow::Result<()> {
        // Only a reply's text is saved; a command's output arrives with its result.
        let stale: Vec<_> = self
            .live
            .values()
            .filter(|live| live.stream == TEXT && live.persisted < live.text.len())
            .map(|live| (live.item.id.clone(), live.text.clone()))
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        let mut tx = self.app.store.tx(self.lease.org_id).await?;
        for (item, text) in &stale {
            sqlx::query(
                "UPDATE runs SET response_text=$4 WHERE org_id=$1 AND assistant_item_id=$2
                 AND exec_epoch=$3 AND state IN ('running','requires_action')",
            )
            .bind(self.lease.org_id)
            .bind(Uuid::parse_str(item)?)
            .bind(self.lease.epoch)
            .bind(text)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        for (item, text) in stale {
            if let Some(live) = self.live.get_mut(&(item, TEXT)) {
                live.persisted = live.persisted.max(text.len());
            }
        }
        Ok(())
    }

    /// Forget streams of items that are no longer streaming or running.
    async fn prune_live(&mut self) -> anyhow::Result<()> {
        if self.live.is_empty() {
            return Ok(());
        }
        let mut tx = self.app.store.tx(self.lease.org_id).await?;
        let streaming: HashSet<String> = sqlx::query_scalar(
            "SELECT id::text FROM items WHERE org_id=$1 AND conversation_id=$2
             AND state IN ('streaming','running','planned')",
        )
        .bind(self.lease.org_id)
        .bind(self.lease.conversation_id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
        tx.commit().await?;
        self.live.retain(|(id, _), _| streaming.contains(id));
        Ok(())
    }

    async fn reply_folds(&mut self, request: async_nats::Message) -> anyhow::Result<()> {
        let Some(reply) = request.reply else {
            return Ok(());
        };
        let channel = self.lease.channel();
        let frames = self
            .live
            .values()
            .map(|live| {
                let mut fold = live.fold(&channel);
                fold.writer_epoch = self.lease.epoch as u64;
                fold
            })
            .collect();
        let batch = FrameBatch { frames };
        self.app
            .nats
            .publish(reply, batch.encode_to_vec().into())
            .await?;
        Ok(())
    }
}
