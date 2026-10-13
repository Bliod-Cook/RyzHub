//! A client for remote MCP servers. Servers on the 2026-07-28 protocol are stateless: every
//! request carries the protocol version and the client's capabilities, found with
//! `server/discover`. Older servers get the `initialize` handshake and a session.
use anyhow::{Context, ensure};
use reqwest::{
    Method,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use url::Url;

use super::fetch::{Fetch, read};
use crate::error::Code;

pub const MODERN: &str = "2026-07-28";
pub const LEGACY: &str = "2025-11-25";
/// Largest answer read.
const MAX_REPLY: usize = 8 * 1024 * 1024;
/// Pages of tools read at most.
const MAX_PAGES: usize = 20;

/// The server answered 401: an account must be connected (OAuth). Carries where to start.
#[derive(Debug, thiserror::Error)]
#[error("PLUGIN_AUTH_REQUIRED")]
pub struct AuthRequired {
    /// The protected resource metadata the server named, if any.
    pub resource_metadata: Option<String>,
    pub scope: Option<String>,
}

/// What a tool call came back with.
#[derive(Debug)]
pub enum Reply {
    Done(Value),
    /// The server needs answers first (elicitation): the requests, keyed, and the state to
    /// send back with the answers.
    InputRequired {
        requests: Value,
        state: Option<String>,
    },
}

pub struct Session {
    fetch: Fetch,
    url: Url,
    headers: HeaderMap,
    /// The negotiated protocol; `MODERN` needs no session.
    version: String,
    session: Option<HeaderValue>,
    next: AtomicU64,
}

fn client_info() -> Value {
    json!({"name": "ryzhub", "version": env!("CARGO_PKG_VERSION")})
}

fn capabilities() -> Value {
    json!({"elicitation": {"form": {}, "url": {}}})
}

impl Session {
    /// Find how to talk to the server at `url`; `headers` go with every request (settings,
    /// account tokens).
    pub async fn open(fetch: &Fetch, url: &str, headers: HeaderMap) -> anyhow::Result<Self> {
        let mut session = Self {
            fetch: fetch.clone(),
            url: Url::parse(url).context(Code::UnsafeDestination)?,
            headers,
            version: MODERN.into(),
            session: None,
            next: AtomicU64::new(1),
        };
        match session.request("server/discover", json!({})).await {
            Ok(result)
                if result["supportedVersions"]
                    .as_array()
                    .is_some_and(|v| v.iter().any(|v| v == MODERN)) =>
            {
                return Ok(session);
            }
            Err(error) if error.downcast_ref::<AuthRequired>().is_some() => return Err(error),
            _ => {}
        }
        // A server before the stateless protocol: shake hands.
        session.version = LEGACY.into();
        let (result, id) = session
            .exchange(
                "initialize",
                json!({"protocolVersion": LEGACY, "capabilities": capabilities(), "clientInfo": client_info()}),
            )
            .await?;
        session.session = id;
        if let Some(version) = result["protocolVersion"].as_str() {
            session.version = version.into();
        }
        session
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(session)
    }

    fn headers(&self) -> HeaderMap {
        let mut headers = self.headers.clone();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert(
            "accept",
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if let Ok(version) = HeaderValue::from_str(&self.version) {
            headers.insert(HeaderName::from_static("mcp-protocol-version"), version);
        }
        if let Some(session) = &self.session {
            headers.insert(HeaderName::from_static("mcp-session-id"), session.clone());
        }
        headers
    }

    /// Params with the per-request metadata the stateless protocol requires.
    fn params(&self, mut params: Value) -> Value {
        if self.version == MODERN {
            params["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!(MODERN);
            params["_meta"]["io.modelcontextprotocol/clientCapabilities"] = capabilities();
            params["_meta"]["io.modelcontextprotocol/clientInfo"] = client_info();
        }
        params
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let response = self
            .fetch
            .open(
                Method::POST,
                &self.url,
                self.headers(),
                Some(body.to_string().into()),
            )
            .await?;
        ensure!(response.status().is_success(), Code::McpUpstreamFailed);
        Ok(())
    }

    /// One request; its result.
    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        Ok(self.exchange(method, params).await?.0)
    }

    /// One request; its result and the session the server started, if it did.
    async fn exchange(
        &self,
        method: &str,
        params: Value,
    ) -> anyhow::Result<(Value, Option<HeaderValue>)> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let body =
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": self.params(params)});
        let response = self
            .fetch
            .open(
                Method::POST,
                &self.url,
                self.headers(),
                Some(body.to_string().into()),
            )
            .await?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get("www-authenticate")
                .and_then(|h| h.to_str().ok())
                .unwrap_or_default();
            return Err(AuthRequired {
                resource_metadata: parameter(challenge, "resource_metadata"),
                scope: parameter(challenge, "scope"),
            }
            .into());
        }
        ensure!(status.is_success(), Code::McpUpstreamFailed);
        let session = response.headers().get("mcp-session-id").cloned();
        let streaming = response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h.starts_with("text/event-stream"));
        let message = if streaming {
            self.read_stream(response, id).await?
        } else {
            serde_json::from_slice(&read(response, MAX_REPLY).await?)
                .context(Code::McpUpstreamFailed)?
        };
        if let Some(error) = message.get("error") {
            anyhow::bail!(McpError(error.clone()));
        }
        Ok((message["result"].clone(), session))
    }

    /// The answer to request `id` in a stream of events. Requests the server makes meanwhile
    /// (older servers' elicitation) are declined.
    async fn read_stream(&self, response: reqwest::Response, id: u64) -> anyhow::Result<Value> {
        use futures_util::StreamExt;
        let mut decoder = crate::provider::SseDecoder::default();
        let mut stream = response.bytes_stream();
        let mut read = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context(Code::McpUpstreamFailed)?;
            read += chunk.len();
            ensure!(read <= MAX_REPLY, Code::McpUpstreamFailed);
            for event in decoder.push(&chunk).context(Code::McpUpstreamFailed)? {
                if event["id"] == id
                    && (event.get("result").is_some() || event.get("error").is_some())
                {
                    return Ok(event);
                }
                if event.get("method").is_some() && event.get("id").is_some() {
                    let answer =
                        json!({"jsonrpc":"2.0","id":event["id"],"result":{"action":"decline"}});
                    let _ = self
                        .fetch
                        .open(
                            Method::POST,
                            &self.url,
                            self.headers(),
                            Some(answer.to_string().into()),
                        )
                        .await;
                }
            }
        }
        anyhow::bail!(Code::McpUpstreamFailed)
    }

    /// Every tool the server offers.
    pub async fn tools(&self) -> anyhow::Result<Vec<Value>> {
        let mut tools = vec![];
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let page = self.request("tools/list", params).await?;
            tools.extend(page["tools"].as_array().cloned().unwrap_or_default());
            match page["nextCursor"].as_str() {
                Some(next) if !next.is_empty() => cursor = Some(next.into()),
                _ => break,
            }
        }
        Ok(tools)
    }

    /// Call `name`. `answers` are the responses to an earlier `input_required`, with its state.
    pub async fn call(
        &self,
        name: &str,
        arguments: Value,
        answers: Option<(Value, Option<String>)>,
    ) -> anyhow::Result<Reply> {
        let mut params = json!({"name": name, "arguments": arguments});
        if let Some((responses, state)) = answers {
            params["inputResponses"] = responses;
            if let Some(state) = state {
                params["requestState"] = json!(state);
            }
        }
        let result = self.request("tools/call", params).await?;
        Ok(if result["resultType"] == "input_required" {
            Reply::InputRequired {
                requests: result["inputRequests"].clone(),
                state: result["requestState"].as_str().map(str::to_owned),
            }
        } else {
            Reply::Done(result)
        })
    }

    /// A resource (an MCP App's `ui://` page).
    pub async fn resource(&self, uri: &str) -> anyhow::Result<Value> {
        self.request("resources/read", json!({"uri": uri})).await
    }
}

/// An error the server answered with.
#[derive(Debug, thiserror::Error)]
#[error("MCP_UPSTREAM_FAILED: {0}")]
pub struct McpError(pub Value);

/// A parameter of a `WWW-Authenticate` challenge.
fn parameter(challenge: &str, name: &str) -> Option<String> {
    let start = challenge.find(&format!("{name}="))? + name.len() + 1;
    let rest = &challenge[start..];
    let value = if let Some(quoted) = rest.strip_prefix('"') {
        &quoted[..quoted.find('"')?]
    } else {
        rest.split([',', ' ']).next()?
    };
    Some(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_name_their_metadata() {
        let challenge = r#"Bearer error="invalid_token", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", scope="read write""#;
        assert_eq!(
            parameter(challenge, "resource_metadata").as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(parameter(challenge, "scope").as_deref(), Some("read write"));
        assert_eq!(parameter("Bearer", "scope"), None);
    }
}
