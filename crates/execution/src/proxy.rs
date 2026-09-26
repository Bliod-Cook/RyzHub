//! The sandbox credential exists only in guest memory. The only upstream is a bound gateway socket.
use crate::guest::Guest;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use std::sync::Arc;
pub fn router(guest: Arc<Guest>) -> Router {
    Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(messages))
        .route("/v1/responses/compact", post(messages))
        .route("/mcp", post(messages))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(guest)
}
async fn messages(
    State(guest): State<Arc<Guest>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((key, socket_name)) = guest.key.read().await.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "credential unavailable").into_response();
    };
    let result = async {
        let client = reqwest::Client::builder()
            .unix_socket(format!("/gateway/{socket_name}"))
            .redirect(reqwest::redirect::Policy::none())
            .read_timeout(std::time::Duration::from_secs(600))
            .build()?;
        let mut request = client
            .post(format!("http://gateway{}", uri.path()))
            .bearer_auth(key)
            .header("content-type", "application/json")
            .body(body);
        for name in [
            "mcp-session-id",
            "mcp-protocol-version",
            "anthropic-version",
            "anthropic-beta",
            "x-claude-code-session-id",
            "x-claude-code-agent-id",
            "x-claude-code-parent-agent-id",
        ] {
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
