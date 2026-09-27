//! Push notifications for authorization-relevant changes.
//!
//! Database triggers (migration 0012) `NOTIFY ryzhub_revocation` whenever a membership, session,
//! credential, gateway key, run, sandbox, conversation, policy or channel writer changes. One
//! `LISTEN` connection per process fans the notices out in memory, so long-lived streams
//! re-validate immediately instead of polling. Subscribers keep a slow periodic check as a
//! fallback for a dropped listener connection.
use serde::Deserialize;
use sqlx::PgPool;
use std::sync::{Arc, OnceLock};
use tokio::sync::broadcast;
use uuid::Uuid;

pub const CHANNEL: &str = "ryzhub_revocation";

/// One changed row. Fields are present when the source table carries them.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Notice {
    pub table: String,
    pub org_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub conversation_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
    /// Channel id for writer changes (`conv:<id>`).
    pub channel: Option<String>,
    pub writer_epoch: Option<i64>,
}

impl Notice {
    /// Could this change affect something scoped to `org`? Unscoped rows (users, sessions)
    /// affect every organization.
    pub fn touches_org(&self, org: Uuid) -> bool {
        self.org_id.is_none_or(|id| id == org)
    }
}

#[derive(Clone)]
pub struct Revocations {
    inner: Arc<Inner>,
}

struct Inner {
    sender: broadcast::Sender<Notice>,
    pool: PgPool,
    started: OnceLock<()>,
}

impl Revocations {
    pub fn new(pool: PgPool) -> Self {
        let (sender, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Inner {
                sender,
                pool,
                started: OnceLock::new(),
            }),
        }
    }

    /// The listener starts with the first subscriber and holds one pooled connection.
    pub fn subscribe(&self) -> broadcast::Receiver<Notice> {
        let receiver = self.inner.sender.subscribe();
        self.inner.started.get_or_init(|| {
            tokio::spawn(listen(self.inner.pool.clone(), self.inner.sender.clone()));
        });
        receiver
    }
}

async fn listen(pool: PgPool, sender: broadcast::Sender<Notice>) {
    let mut backoff = std::time::Duration::from_millis(200);
    loop {
        match listen_once(&pool, &sender).await {
            Ok(()) => backoff = std::time::Duration::from_millis(200),
            Err(error) => {
                tracing::warn!(%error, "revocation listener disconnected");
                // Subscribers treat a lagged receiver as "re-check everything".
                let _ = sender.send(Notice {
                    table: "listener_reset".into(),
                    ..Default::default()
                });
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(10));
            }
        }
    }
}

async fn listen_once(pool: &PgPool, sender: &broadcast::Sender<Notice>) -> anyhow::Result<()> {
    let mut listener = sqlx::postgres::PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    loop {
        let notification = listener.recv().await?;
        match serde_json::from_str::<Notice>(notification.payload()) {
            Ok(notice) => {
                let _ = sender.send(notice);
            }
            Err(error) => tracing::warn!(%error, "malformed revocation notice"),
        }
    }
}
