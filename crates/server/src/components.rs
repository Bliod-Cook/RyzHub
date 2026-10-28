//! What runs in this process, for the admin panel: background loops, each supervised as a
//! task of its own (restarted with backoff when it fails, panics or an admin restarts it),
//! and the listeners, which run as long as the process.
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Notify, task::AbortHandle};

/// The exit status that asks whatever runs the process (Compose's restart policy,
/// `scripts/dev-live.sh`) to start it again.
pub const EXIT_RESTART: i32 = 75;

static RESTART: Notify = Notify::const_new();

/// Ask the process to stop as on SIGTERM, with [`EXIT_RESTART`].
pub fn request_restart() {
    RESTART.notify_one();
}

/// Once the process has been asked to restart.
pub async fn restart_requested() {
    RESTART.notified().await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    /// Waiting to start again after it stopped.
    Backoff,
}

#[derive(Serialize)]
pub struct Status {
    pub name: &'static str,
    pub state: State,
    /// Why it last stopped.
    pub error: Option<String>,
    pub restarts: u32,
    pub started_at_ms: Option<i64>,
    /// Whether an admin may restart it on its own.
    pub restartable: bool,
}

struct Entry {
    status: Status,
    task: Option<AbortHandle>,
    wake: Arc<Notify>,
}

#[derive(Clone)]
pub struct Components {
    /// The role this process serves.
    pub role: String,
    pub started_at: DateTime<Utc>,
    entries: Arc<Mutex<BTreeMap<&'static str, Entry>>>,
}

impl Components {
    pub fn new(role: &str) -> Self {
        Self {
            role: role.into(),
            started_at: Utc::now(),
            entries: Default::default(),
        }
    }

    /// Note a listener, which runs as long as the process.
    pub fn listening(&self, name: &'static str) {
        let wake = Arc::new(Notify::new());
        let status = Status {
            name,
            state: State::Running,
            error: None,
            restarts: 0,
            started_at_ms: Some(Utc::now().timestamp_millis()),
            restartable: false,
        };
        let entry = Entry {
            status,
            task: None,
            wake,
        };
        self.entries.lock().unwrap().insert(name, entry);
    }

    /// Run the loop `start` makes for as long as the process runs. When it fails, panics or
    /// returns it starts again, after a pause that grows while it keeps failing; restarted on
    /// request, it is stopped at its next await point and starts again at once.
    pub fn supervise<F, Fut>(&self, name: &'static str, restartable: bool, start: F)
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let wake = Arc::new(Notify::new());
        let status = Status {
            name,
            state: State::Backoff,
            error: None,
            restarts: 0,
            started_at_ms: None,
            restartable,
        };
        let entry = Entry {
            status,
            task: None,
            wake: wake.clone(),
        };
        self.entries.lock().unwrap().insert(name, entry);
        let entries = self.entries.clone();
        let update = move |change: &dyn Fn(&mut Entry)| {
            if let Some(entry) = entries.lock().unwrap().get_mut(name) {
                change(entry);
            }
        };
        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(500);
            loop {
                let started = tokio::time::Instant::now();
                let task = tokio::spawn(start());
                update(&|entry| {
                    entry.task = Some(task.abort_handle());
                    entry.status.state = State::Running;
                    entry.status.started_at_ms = Some(Utc::now().timestamp_millis());
                });
                let error = match task.await {
                    Err(stopped) if stopped.is_cancelled() => {
                        tracing::info!(task = name, "background task restarted");
                        update(&|entry| entry.status.restarts += 1);
                        continue;
                    }
                    Err(panic) => format!("panicked: {panic}"),
                    Ok(Ok(())) => "returned".into(),
                    Ok(Err(error)) => format!("{error:#}"),
                };
                tracing::error!(task = name, error, "background task stopped; restarting");
                update(&|entry| {
                    entry.task = None;
                    entry.status.state = State::Backoff;
                    entry.status.error = Some(error.clone());
                    entry.status.restarts += 1;
                });
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = Duration::from_millis(500);
                }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = wake.notified() => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });
    }

    /// Restart a restartable loop at once; false for one that is not.
    pub fn restart(&self, name: &str) -> bool {
        let entries = self.entries.lock().unwrap();
        let Some(entry) = entries.get(name).filter(|entry| entry.status.restartable) else {
            return false;
        };
        match &entry.task {
            Some(task) if entry.status.state == State::Running => task.abort(),
            _ => entry.wake.notify_one(),
        }
        true
    }

    /// Every component's status, by name.
    pub fn list(&self) -> Vec<serde_json::Value> {
        let entries = self.entries.lock().unwrap();
        entries
            .values()
            .map(|entry| serde_json::to_value(&entry.status).expect("status as JSON"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    async fn until(components: &Components, check: impl Fn(&serde_json::Value) -> bool) {
        for _ in 0..200 {
            if components.list().iter().any(&check) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{:?}", components.list());
    }

    #[tokio::test]
    async fn a_restarted_or_panicked_loop_starts_again() {
        let components = Components::new("all");
        let starts = Arc::new(AtomicU32::new(0));
        let counted = starts.clone();
        components.supervise("waits", true, move || {
            counted.fetch_add(1, Ordering::SeqCst);
            std::future::pending()
        });
        until(&components, |c| c["state"] == "running").await;
        assert!(components.restart("waits"));
        until(&components, |c| {
            c["restarts"] == 1 && c["state"] == "running"
        })
        .await;
        assert_eq!(starts.load(Ordering::SeqCst), 2);

        let panics = Arc::new(AtomicU32::new(0));
        let counted = panics.clone();
        components.supervise("panics", false, move || {
            let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
            async move {
                if first {
                    panic!("boom");
                }
                std::future::pending().await
            }
        });
        until(&components, |c| {
            c["name"] == "panics" && c["restarts"] == 1 && c["state"] == "running"
        })
        .await;
        let listed = components.list();
        let panicked = listed.iter().find(|c| c["name"] == "panics").unwrap();
        assert!(panicked["error"].as_str().unwrap().contains("panicked"));
        assert!(!components.restart("panics"), "not restartable");
        assert!(!components.restart("unknown"));
    }
}
