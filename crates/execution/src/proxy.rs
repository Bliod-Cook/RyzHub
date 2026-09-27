//! Local credential proxy on 127.0.0.1:8090. The sandbox key exists only in guest memory and
//! is attached here; the only upstream is the gateway socket bind-mounted at `/gateway`.
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;

use crate::guest::Guest;

/// Model streams may be silent while the provider thinks.
const MODEL_READ_TIMEOUT: Duration = Duration::from_secs(600);
/// MCP calls may wait for a user approval.
const MCP_READ_TIMEOUT: Duration = Duration::from_secs(25 * 3600);

const FORWARDED: [&str; 7] = [
    "mcp-session-id",
    "mcp-protocol-version",
    "anthropic-version",
    "anthropic-beta",
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
];

struct Proxy {
    guest: Arc<Guest>,
    /// Pooled clients per gateway socket: (model, mcp).
    clients: Mutex<HashMap<String, (reqwest::Client, reqwest::Client)>>,
}

pub fn router(guest: Arc<Guest>) -> anyhow::Result<Router> {
    let proxy = Arc::new(Proxy {
        guest,
        clients: Mutex::new(HashMap::new()),
    });
    Ok(Router::new()
        .route("/v1/messages", post(forward))
        .route("/v1/responses", post(forward))
        .route("/v1/responses/compact", post(forward))
        .route("/mcp", post(forward))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(proxy))
}

impl Proxy {
    async fn client(&self, socket_name: &str, mcp: bool) -> anyhow::Result<reqwest::Client> {
        let mut clients = self.clients.lock().await;
        if !clients.contains_key(socket_name) {
            let build = |read| {
                reqwest::Client::builder()
                    .unix_socket(format!("/gateway/{socket_name}"))
                    .redirect(reqwest::redirect::Policy::none())
                    .read_timeout(read)
                    .build()
            };
            clients.insert(
                socket_name.into(),
                (build(MODEL_READ_TIMEOUT)?, build(MCP_READ_TIMEOUT)?),
            );
        }
        let (model, tools) = &clients[socket_name];
        Ok(if mcp { tools.clone() } else { model.clone() })
    }
}

async fn forward(
    State(proxy): State<Arc<Proxy>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((key, socket_name)) = proxy.guest.key.read().await.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "credential unavailable").into_response();
    };
    let result = async {
        let client = proxy.client(&socket_name, uri.path() == "/mcp").await?;
        let mut request = client
            .post(format!("http://gateway{}", uri.path()))
            .bearer_auth(key)
            .header("content-type", "application/json")
            .body(body);
        for name in FORWARDED {
            if let Some(value) = headers.get(name) {
                request = request.header(name, value);
            }
        }
        let upstream = request.send().await?;
        let mut response = Response::builder().status(upstream.status());
        for name in ["content-type", "request-id", "retry-after"] {
            if let Some(value) = upstream.headers().get(name) {
                response = response.header(name, value);
            }
        }
        Ok::<_, anyhow::Error>(response.body(Body::from_stream(upstream.bytes_stream()))?)
    }
    .await;
    result.unwrap_or_else(|_| (StatusCode::BAD_GATEWAY, "gateway unavailable").into_response())
}
