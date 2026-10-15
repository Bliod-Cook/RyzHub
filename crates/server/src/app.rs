//! Process-wide state shared by every role.
use std::{sync::Arc, time::Duration};

use crate::{
    config::Config,
    revocation::Revocations,
    store::Store,
    ws::{Hub, sse::Streams},
};

#[derive(Clone)]
pub struct App {
    pub store: Store,
    pub nats: async_nats::Client,
    pub config: Arc<Config>,
    pub http: Http,
    /// One NATS subscription per channel per process, shared by all local subscribers.
    pub hub: Hub,
    pub revocations: Revocations,
    /// Event-stream connections opened here.
    pub streams: Streams,
}

impl App {
    pub fn new(store: Store, nats: async_nats::Client, config: Config) -> anyhow::Result<Self> {
        let revocations = Revocations::new(store.pool.clone());
        let hub = Hub::new(store.clone(), nats.clone(), revocations.clone());
        Ok(Self {
            http: Http::new()?,
            store,
            nats,
            config: Arc::new(config),
            hub,
            revocations,
            streams: Streams::default(),
        })
    }
}

/// Pooled HTTP clients. Building a client per request discards connection reuse and TLS
/// sessions, so every outbound path shares one of these.
#[derive(Clone)]
pub struct Http {
    /// Model providers. Redirects are never followed with a credential attached.
    pub upstream: reqwest::Client,
    /// Owner to LLM gateway.
    pub gateway: reqwest::Client,
    /// Remote MCP tool servers.
    pub tools: reqwest::Client,
    /// Sign-in providers the instance configures.
    pub identity: reqwest::Client,
}

/// Longest silence in a model response before the request fails.
const MODEL_READ_TIMEOUT: Duration = Duration::from_secs(120);

impl Http {
    fn new() -> anyhow::Result<Self> {
        let read = MODEL_READ_TIMEOUT;
        Ok(Self {
            upstream: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(read)
                .build()?,
            identity: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .user_agent(concat!("RyzHub/", env!("CARGO_PKG_VERSION")))
                .build()?,
            gateway: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(read)
                .build()?,
            tools: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(60))
                .build()?,
        })
    }
}
