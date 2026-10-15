//! Conversation Owners.
//!
//! Each active conversation has exactly one [`actor`] in the cluster, holding the channel lease.
//! An Owner process claims conversations when woken over NATS (a command was queued) and on a
//! slow reconciliation scan that reads only active rows. Actors are event driven; nothing polls
//! at sub-second intervals.
mod actor;
mod commands;
mod publish;

pub use actor::{LiveRef, OwnerLink, TERMINAL_OUTPUT, TEXT, WorkerEvent};
pub use commands::{Processed, process_next, process_with_config};
pub use publish::{publish, publish_transient};

use futures_util::StreamExt;
use std::{collections::HashMap, time::Duration};
use tokio::{
    sync::Notify,
    task::{self, JoinSet},
};
use uuid::Uuid;

use crate::{app::App, config::Config, store::Lease};

/// Conversation Owner lease; renewed every third of it.
const LEASE_MS: u64 = 3000;
/// How often an Owner process looks for conversations that need an actor without a wake-up.
const RECONCILE: Duration = Duration::from_secs(1);
/// How often user and organization channels are published.
const OUTBOX: Duration = Duration::from_millis(500);
/// Claims an Owner process makes at once; more wait for the next reconciliation.
const MAX_CLAIMS: usize = 16;

pub fn subject(channel: &str) -> String {
    format!("ryzhub.events.{}", channel.replace(':', "."))
}

pub fn fold_subject(channel: &str) -> String {
    format!("ryzhub.fold.{}", channel.replace(':', "."))
}

/// Published after a command is queued; the Owner holding the conversation processes it.
pub fn wake_subject(org: Uuid, conv: Uuid) -> String {
    format!("ryzhub.wake.{org}.{conv}")
}

/// The stored result of a processed command, pushed to the gateway waiting for it.
pub fn result_subject(user: Uuid, cid: Uuid) -> String {
    format!("ryzhub.result.{user}.{cid}")
}

/// What drives a conversation's runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    /// The platform's own agent loop.
    Chat,
    /// A native harness in a persistent sandbox.
    Work,
}

impl Engine {
    pub fn of(mode: &str) -> Self {
        if mode == "work" {
            Self::Work
        } else {
            Self::Chat
        }
    }
}

/// Discover and claim conversations, and publish user/org channels.
pub async fn run(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    let owner = Uuid::now_v7();
    let mut actors: HashMap<Uuid, std::sync::Arc<Notify>> = HashMap::new();
    let mut running: JoinSet<Uuid> = JoinSet::new();
    let mut claims = Claims::default();
    let mut wakes = app.nats.subscribe("ryzhub.wake.>").await?;
    let mut reconcile = tokio::time::interval(RECONCILE);
    let mut outbox = tokio::time::interval(OUTBOX);
    loop {
        tokio::select! {
            Some(message) = wakes.next() => {
                let Some((org, conv)) = parse_wake(&message.subject) else { continue };
                match actors.get(&conv) {
                    Some(wake) => wake.notify_one(),
                    None => claims.start(&app, owner, org, conv),
                }
            }
            _ = reconcile.tick() => {
                for (org, conv) in active_conversations(&worker, &app.config).await? {
                    if !actors.contains_key(&conv) {
                        claims.start(&app, owner, org, conv);
                    }
                }
            }
            _ = outbox.tick() => publish_outbox(&app, &worker).await?,
            Some(claimed) = claims.tasks.join_next_with_id() => {
                if let Some((conv, Some(lease))) = claims.finished(claimed) {
                    start(&app, conv, lease, &mut actors, &mut running);
                }
            }
            Some(ended) = running.join_next() => {
                if let Ok(conv) = ended {
                    actors.remove(&conv);
                }
            }
        }
    }
}

/// Claims under way, each in a task of its own: one waiting on its conversation holds up
/// neither the wake-ups and claims of others nor the outbox.
#[derive(Default)]
struct Claims {
    tasks: JoinSet<Option<Lease>>,
    by_task: HashMap<task::Id, Uuid>,
}

impl Claims {
    /// Claim `conv` unless it is being claimed already or too many claims are under way.
    fn start(&mut self, app: &App, owner: Uuid, org: Uuid, conv: Uuid) {
        if self.by_task.len() >= MAX_CLAIMS || self.by_task.values().any(|c| *c == conv) {
            return;
        }
        let app = app.clone();
        let handle = self.tasks.spawn(async move {
            match app.store.claim(org, conv, owner, LEASE_MS).await {
                Ok(lease) => lease,
                Err(error) => {
                    tracing::warn!(%conv, %error, "claim failed");
                    None
                }
            }
        });
        self.by_task.insert(handle.id(), conv);
    }

    /// The conversation a finished claim was for, with its lease if it won one.
    fn finished(
        &mut self,
        claimed: Result<(task::Id, Option<Lease>), task::JoinError>,
    ) -> Option<(Uuid, Option<Lease>)> {
        match claimed {
            Ok((id, lease)) => Some((self.by_task.remove(&id)?, lease)),
            Err(error) => {
                self.by_task.remove(&error.id());
                None
            }
        }
    }
}

fn parse_wake(subject: &str) -> Option<(Uuid, Uuid)> {
    let mut parts = subject.strip_prefix("ryzhub.wake.")?.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Run the actor of a conversation this Owner has just claimed.
fn start(
    app: &App,
    conv: Uuid,
    lease: Lease,
    actors: &mut HashMap<Uuid, std::sync::Arc<Notify>>,
    running: &mut JoinSet<Uuid>,
) {
    let wake = std::sync::Arc::new(Notify::new());
    // A wake that raced the claim still finds its command on the first pass.
    wake.notify_one();
    actors.insert(conv, wake.clone());
    let app = app.clone();
    running.spawn(async move {
        if let Err(error) = actor::run(app, lease, wake).await {
            tracing::warn!(%conv, error = format!("{error:#}"), "owner actor stopped");
        }
        conv
    });
}

/// Conversations with queued commands, active runs, pending jobs, live chat sandboxes,
/// unpublished events, or Work sandboxes that need a checkpoint, suspension or recovery.
/// Each branch reads a partial index over active rows only.
///
/// An idle Owner has exited, so this is what wakes a workspace for suspension. The idle limit
/// is the organization's effective one, as [`Config::idle_suspend_secs`] computes it: the
/// smallest of the instance setting (0 = none), the instance policy and the organization
/// policy; no limit disables suspension.
pub async fn active_conversations(
    worker: &sqlx::PgPool,
    config: &Config,
) -> anyhow::Result<Vec<(Uuid, Uuid)>> {
    // TODO(org): the latest organization policy version; only registration writes one.
    Ok(sqlx::query_as(
        "SELECT org_id,conversation_id FROM commands WHERE status='queued'
         UNION SELECT org_id,conversation_id FROM runs WHERE state IN ('running','requires_action')
         UNION SELECT org_id,conversation_id FROM workspace_jobs WHERE state IN ('queued','running')
         UNION SELECT org_id,conversation_id FROM conversation_operations WHERE state IN ('queued','running')
         UNION SELECT org_id,conversation_id FROM channels WHERE kind='conv' AND last_seq>published_seq
         UNION SELECT s.org_id,s.conversation_id FROM sandboxes s
           CROSS JOIN LATERAL (SELECT least(nullif($1::bigint,0), $2::bigint,
               (SELECT CASE WHEN jsonb_typeof(p.policy->'compute_idle_ttl_secs')='number'
                            THEN (p.policy->>'compute_idle_ttl_secs')::numeric::bigint END
                FROM org_policies p WHERE p.org_id=s.org_id ORDER BY p.version DESC LIMIT 1)) AS secs) idle
           WHERE s.state IN ('running','provisioning')
           -- A Chat sandbox kept for reuse needs nothing until it expires.
           AND ((NOT persistent AND coalesce(s.idle_until<=now(),true))
                OR persistent AND (node_lease_until<clock_timestamp()
                OR EXISTS(SELECT 1 FROM runs r WHERE r.org_id=s.org_id AND r.sandbox_id=s.id
                          AND r.state IN ('cancelled','failed') AND r.native_turn_id IS NOT NULL
                          AND NOT r.native_interrupted)
                OR EXISTS(SELECT 1 FROM runs r WHERE r.org_id=s.org_id AND r.conversation_id=s.conversation_id
                          AND r.state='completed' AND NOT r.read_only
                          AND r.ended_at>coalesce((SELECT max(c.created_at) FROM checkpoints c
                              WHERE c.org_id=s.org_id AND c.conversation_id=s.conversation_id
                                AND c.state IN ('uploading','committed')), '-infinity'))
                OR (idle.secs>0 AND s.created_at<now()-(idle.secs * interval '1 second')
                    AND coalesce(s.terminal_seen_at<now()-interval '1 minute',true)
                    AND NOT EXISTS(SELECT 1 FROM runs r WHERE r.org_id=s.org_id AND r.conversation_id=s.conversation_id
                        AND coalesce(r.ended_at,'infinity')>now()-(idle.secs * interval '1 second')))))",
    )
    .bind(config.work.idle_suspend_secs as i64)
    .bind(config.policy.compute_idle_ttl_secs.map(|secs| secs as i64))
    .fetch_all(worker)
    .await?)
}

/// User and organization channels have no Owner; publish them from the outbox.
async fn publish_outbox(app: &App, worker: &sqlx::PgPool) -> anyhow::Result<()> {
    let channels: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT org_id,id FROM channels WHERE kind<>'conv' AND last_seq>published_seq",
    )
    .fetch_all(worker)
    .await?;
    for (org, channel) in channels {
        if let Err(error) = publish(&app.store, &app.nats, org, &channel, None, vec![]).await {
            tracing::warn!(channel, %error, "outbox publication deferred");
        }
    }
    Ok(())
}
