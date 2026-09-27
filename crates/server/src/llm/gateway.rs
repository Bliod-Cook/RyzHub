//! Native-protocol model proxy. Requests pass through unchanged apart from authentication;
//! usage is read from the stream as it is forwarded.
use anyhow::{Context, ensure};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, State},
    http::HeaderMap,
    response::Response,
    routing::post,
};
use futures_util::StreamExt;
use ryzhub_domain::{Action, Policy, Principal, Resource, authorize};
use serde_json::{Value, json};
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

use super::{
    credentials,
    keys::{self, GatewayKey, KeyKind},
};
use crate::{
    app::App,
    error::{ApiResult, Code},
    provider::{NativeMessage, ResponsesUsage, SseDecoder},
    store::{Tx, context_in, effective_policy},
};

/// Fallback re-validation of an accepted stream when no revocation notice arrives.
const STREAM_RECHECK: Duration = Duration::from_secs(5);

pub fn router(app: App) -> Router {
    Router::new()
        .route("/mcp", post(crate::mcp::endpoint))
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .route("/v1/responses/compact", post(compact))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(app)
}

pub fn bearer(headers: &HeaderMap) -> anyhow::Result<&str> {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .context(Code::Unauthorized)
}

/// What a valid key is currently bound to.
pub struct Binding {
    /// The credential the caller asked for explicitly (run or key selection).
    pub credential: Option<Uuid>,
}

/// Identity and liveness: the caller is still a member, the run is active, and the key's
/// binding (Owner epoch for loop keys, sandbox lease for sandbox keys) is current. Once a
/// stream has been accepted (`streaming`), expiry and supersession no longer interrupt it.
pub async fn validate_key(
    tx: &mut Tx<'_>,
    key: &GatewayKey,
    streaming: bool,
) -> anyhow::Result<Binding> {
    let ctx = context_in(tx, key.user_id, key.org_id).await?;
    let run = sqlx::query(
        "SELECT r.exec_epoch,r.state,r.credential_id,c.creator_id,c.owner_epoch,
                ch.lease_until>clock_timestamp() AS live,
                v.revoked_at IS NULL AS unrevoked,v.superseded_at IS NULL AS current,v.expires_at>now() AS unexpired
         FROM runs r
             JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id
             JOIN channels ch ON ch.org_id=c.org_id AND ch.conversation_id=c.id
             JOIN virtual_keys v ON v.org_id=r.org_id AND v.id=$4
         WHERE r.org_id=$1 AND r.id=$2 AND r.conversation_id=$3 AND c.state='active'",
    )
    .bind(key.org_id)
    .bind(key.run_id)
    .bind(key.conversation_id)
    .bind(key.id)
    .fetch_optional(&mut **tx)
    .await?
    .context(Code::Forbidden)?;
    ensure!(
        run.get::<Uuid, _>("creator_id") == key.user_id
            && matches!(run.get::<&str, _>("state"), "running" | "requires_action"),
        Code::Forbidden
    );
    let lost = if streaming {
        Code::Fenced
    } else {
        Code::Unauthorized
    };
    ensure!(run.get::<bool, _>("unrevoked"), lost);
    if !streaming {
        ensure!(run.get::<bool, _>("unexpired"), Code::Unauthorized);
    }
    let credential = match key.kind() {
        KeyKind::Loop => {
            let epoch = key.exec_epoch;
            ensure!(
                epoch == Some(run.get("exec_epoch"))
                    && epoch == Some(run.get("owner_epoch"))
                    && run.get::<Option<bool>, _>("live") == Some(true),
                Code::Fenced
            );
            run.get("credential_id")
        }
        KeyKind::Sandbox => {
            if !streaming {
                ensure!(run.get::<bool, _>("current"), Code::Unauthorized);
            }
            let sandbox_live: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM sandboxes WHERE org_id=$1 AND id=$2 AND lease_epoch=$3
                 AND state='running' AND (node_lease_until IS NULL OR node_lease_until>clock_timestamp()))",
            )
            .bind(key.org_id)
            .bind(key.sandbox_id)
            .bind(key.lease_epoch)
            .fetch_one(&mut **tx)
            .await?;
            ensure!(sandbox_live, lost);
            key.credential_id
        }
    };
    authorize(
        &Principal::User(ctx),
        Action::UseModel,
        &Resource {
            org_id: key.org_id,
            id: key.conversation_id,
            owner_user_id: Some(key.user_id),
            target_role: None,
        },
    )
    .map_err(|_| Code::Forbidden)?;
    Ok(Binding { credential })
}

/// The model is allowed for this key and by current policy.
async fn authorize_model(
    app: &App,
    tx: &mut Tx<'_>,
    key: &GatewayKey,
    model: &str,
) -> anyhow::Result<Policy> {
    let policy = effective_policy(tx, &app.config.policy, key.org_id).await?;
    ensure!(
        key.models.iter().any(|m| m == model)
            && policy.models.as_ref().is_none_or(|m| m.contains(model)),
        Code::ModelDenied
    );
    Ok(policy)
}

/// Fixed one-minute windows per organization and per key (loop keys: per run).
async fn rate_limit(app: &App, tx: &mut Tx<'_>, key: &GatewayKey) -> anyhow::Result<()> {
    let scopes = [
        ("org".to_owned(), app.config.llm.requests_per_minute_org),
        (
            match key.kind() {
                KeyKind::Loop => format!("run:{}", key.run_id),
                KeyKind::Sandbox => format!("key:{}", key.id),
            },
            app.config.llm.requests_per_minute_key,
        ),
    ];
    for (scope, limit) in scopes {
        let count: i32 = sqlx::query_scalar(
            "INSERT INTO model_rate_limits(org_id,scope,minute,requests)
             VALUES($1,$2,floor(extract(epoch FROM now())/60)::bigint,1)
             ON CONFLICT(org_id,scope,minute) DO UPDATE SET requests=model_rate_limits.requests+1
             RETURNING requests",
        )
        .bind(key.org_id)
        .bind(scope)
        .fetch_one(&mut **tx)
        .await?;
        ensure!(count <= limit, Code::RateLimited);
    }
    Ok(())
}

async fn messages(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    Ok(proxy(app, headers, body, Provider::Anthropic, "/v1/messages").await?)
}

async fn responses(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    Ok(proxy(app, headers, body, Provider::OpenAi, "/v1/responses").await?)
}

async fn compact(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    Ok(proxy(
        app,
        headers,
        body,
        Provider::OpenAi,
        "/v1/responses/compact",
    )
    .await?)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Provider {
    Anthropic,
    OpenAi,
}

impl Provider {
    fn credential(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }
}

/// Headers forwarded to the provider unchanged.
const FORWARDED: [&str; 5] = [
    "anthropic-version",
    "anthropic-beta",
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
];

async fn proxy(
    app: App,
    headers: HeaderMap,
    body: Bytes,
    provider: Provider,
    path: &str,
) -> anyhow::Result<Response> {
    ensure!(app.config.llm.enabled, Code::ModelGatewayDisabled);
    let key = keys::resolve(&app.store, bearer(&headers)?).await?;
    let request: Value = serde_json::from_slice(&body).map_err(|_| Code::InvalidInput)?;
    let model = request["model"]
        .as_str()
        .context(Code::InvalidInput)?
        .to_owned();
    let streaming = request["stream"].as_bool().unwrap_or(false);
    let mut tx = app.store.tx(key.org_id).await?;
    let binding = validate_key(&mut tx, &key, false).await?;
    let policy = authorize_model(&app, &mut tx, &key, &model).await?;
    let (credential, secret) = credentials::resolve(
        &app,
        &mut tx,
        key.org_id,
        key.user_id,
        binding.credential,
        &policy,
        provider.credential(),
    )
    .await?;
    rate_limit(&app, &mut tx, &key).await?;
    let request_id = Uuid::now_v7();
    let metadata = |name: &str| {
        headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .map(|h| h.chars().take(256).collect::<String>())
    };
    sqlx::query(
        "INSERT INTO usage_records(id,org_id,user_id,run_id,virtual_key_id,model,credential_id,session_id,
             agent_id,parent_agent_id,provider)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
    )
    .bind(request_id)
    .bind(key.org_id)
    .bind(key.user_id)
    .bind(key.run_id)
    .bind((key.kind() == KeyKind::Sandbox).then_some(key.id))
    .bind(&model)
    .bind(credential)
    .bind(metadata("x-claude-code-session-id"))
    .bind(metadata("x-claude-code-agent-id"))
    .bind(metadata("x-claude-code-parent-agent-id"))
    .bind(provider.credential())
    .execute(&mut *tx)
    .await?;
    crate::store::audit_as(
        &mut tx,
        key.org_id,
        key.user_id,
        if key.kind() == KeyKind::Loop {
            "loop"
        } else {
            "sandbox"
        },
        "model.request",
        &request_id.to_string(),
        "accepted",
    )
    .await?;
    tx.commit().await?;

    let base = match provider {
        Provider::OpenAi => &app.config.llm.responses_url,
        Provider::Anthropic => &app.config.llm.upstream_url,
    };
    let mut outgoing = app
        .http
        .upstream
        .post(format!("{}{path}", base.trim_end_matches('/')))
        .header("content-type", "application/json")
        .body(body.clone());
    outgoing = match provider {
        Provider::OpenAi => outgoing.bearer_auth(secret),
        Provider::Anthropic => outgoing.header("x-api-key", secret),
    };
    for name in FORWARDED {
        if let Some(value) = headers.get(name) {
            outgoing = outgoing.header(name, value);
        }
    }
    if provider == Provider::Anthropic && !headers.contains_key("anthropic-version") {
        outgoing = outgoing.header("anthropic-version", "2023-06-01");
    }
    let meter = Meter {
        app: app.clone(),
        key,
        model,
        credential,
        request_id,
        input_bytes: body.len(),
        provider,
        streaming,
    };
    let upstream = match outgoing.send().await {
        Ok(response) => response,
        Err(_) => {
            meter
                .finish(Value::Null, 0, false, "upstream_unreachable", None)
                .await?;
            anyhow::bail!(Code::UpstreamUnavailable);
        }
    };
    let status = upstream.status();
    let mut response = Response::builder().status(status);
    for name in ["content-type", "request-id", "x-request-id"] {
        if let Some(value) = upstream.headers().get(name) {
            response = response.header(name, value);
        }
    }
    response = response
        .header("cache-control", "no-store")
        .header("x-accel-buffering", "no");
    if let Some(seconds) = upstream
        .headers()
        .get("retry-after")
        .and_then(|h| h.to_str().ok())
        .map(retry_after_seconds)
    {
        response = response.header("retry-after", seconds.to_string());
    }
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    tokio::spawn(meter.forward(upstream, sender));
    Ok(response.body(Body::from_stream(
        tokio_stream::wrappers::ReceiverStream::new(receiver),
    ))?)
}

/// Integer seconds in [1, 60], as Claude Code expects.
fn retry_after_seconds(value: &str) -> u64 {
    value
        .parse::<u64>()
        .ok()
        .or_else(|| {
            chrono::DateTime::parse_from_rfc2822(value)
                .ok()
                .map(|at| (at.timestamp() - chrono::Utc::now().timestamp()).max(0) as u64)
        })
        .unwrap_or(1)
        .clamp(1, 60)
}

/// Forwards one upstream response, re-validates the caller while it streams, and records usage.
struct Meter {
    app: App,
    key: GatewayKey,
    model: String,
    credential: Option<Uuid>,
    request_id: Uuid,
    input_bytes: usize,
    provider: Provider,
    streaming: bool,
}

impl Meter {
    async fn forward(
        self,
        upstream: reqwest::Response,
        sender: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    ) {
        let status = upstream.status();
        let mut stream = upstream.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut anthropic = NativeMessage::default();
        let mut openai = ResponsesUsage::default();
        let mut notices = self.app.revocations.subscribe();
        let mut recheck = tokio::time::interval(STREAM_RECHECK);
        recheck.tick().await;
        let (mut parse_ok, mut completed, mut received) = (true, true, 0usize);
        let mut json_body = Vec::new();
        loop {
            let chunk = tokio::select! {
                () = sender.closed() => { completed = false; break; }
                notice = notices.recv() => {
                    let relevant = notice.as_ref().map_or(true, |n| n.touches_org(self.key.org_id));
                    if relevant && self.revalidate().await.is_err() {
                        completed = false;
                        let _ = sender.try_send(Err(std::io::Error::other("authorization revoked")));
                        break;
                    }
                    continue;
                }
                _ = recheck.tick() => {
                    if self.revalidate().await.is_err() {
                        completed = false;
                        let _ = sender.try_send(Err(std::io::Error::other("authorization revoked")));
                        break;
                    }
                    continue;
                }
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else { break };
            let Ok(bytes) = chunk else {
                completed = false;
                let _ = sender
                    .send(Err(std::io::Error::other("upstream stream interrupted")))
                    .await;
                break;
            };
            received += bytes.len();
            if self.streaming && status.is_success() {
                if parse_ok {
                    parse_ok = decoder.push(&bytes).is_ok_and(|events| {
                        events.iter().all(|event| match self.provider {
                            Provider::OpenAi => openai.apply(event).is_ok(),
                            Provider::Anthropic => anthropic.apply(event).is_ok(),
                        })
                    });
                }
            } else if json_body.len() + bytes.len() <= 4 * 1024 * 1024 {
                json_body.extend_from_slice(&bytes);
            } else {
                parse_ok = false;
            }
            if sender.send(Ok(bytes)).await.is_err() {
                completed = false;
                break;
            }
        }
        let (usage, final_usage, output_bytes) = match (self.streaming, self.provider) {
            (true, Provider::OpenAi) => (
                openai.usage.clone(),
                completed && parse_ok && openai.finished,
                received,
            ),
            (true, Provider::Anthropic) => (
                anthropic.response["usage"].clone(),
                completed && parse_ok && anthropic.stopped,
                anthropic.text_bytes,
            ),
            (false, _) => {
                let value: Value = serde_json::from_slice(&json_body).unwrap_or(Value::Null);
                (
                    value["usage"].clone(),
                    completed && parse_ok && value["usage"].is_object(),
                    received,
                )
            }
        };
        let outcome = if !status.is_success() || openai.failed {
            "upstream_error"
        } else if completed && final_usage {
            "completed"
        } else {
            "interrupted"
        };
        if let Err(error) = self
            .finish(
                usage,
                output_bytes,
                final_usage,
                outcome,
                Some(status.as_u16() as i32),
            )
            .await
        {
            tracing::error!(request_id = %self.request_id, %error, "usage finalization failed");
        }
        // The response closes only after accounting; a disconnected client still leaves an
        // estimated record.
        drop(sender);
    }

    /// Authorization of an accepted stream: key binding, model policy and credential.
    async fn revalidate(&self) -> anyhow::Result<()> {
        let mut tx = self.app.store.tx(self.key.org_id).await?;
        let result = self.authorized(&mut tx).await;
        // Read-only: end it explicitly so the connection is back in the pool at once.
        tx.rollback().await?;
        result
    }

    async fn authorized(&self, tx: &mut Tx<'_>) -> anyhow::Result<()> {
        validate_key(tx, &self.key, true).await?;
        let policy = effective_policy(tx, &self.app.config.policy, self.key.org_id).await?;
        ensure!(
            policy
                .models
                .as_ref()
                .is_none_or(|m| m.contains(&self.model)),
            Code::ModelDenied
        );
        match self.credential {
            Some(id) => {
                let usable: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM model_credentials WHERE org_id=$1 AND id=$2 AND revoked_at IS NULL
                     AND (owner_user_id IS NULL OR (owner_user_id=$3 AND $4)))",
                )
                .bind(self.key.org_id)
                .bind(id)
                .bind(self.key.user_id)
                .bind(policy.allow_personal_credentials != Some(false))
                .fetch_one(&mut **tx)
                .await?;
                ensure!(usable, Code::CredentialRevoked);
            }
            None => ensure!(
                policy.allow_platform_credentials != Some(false),
                Code::PlatformCredentialDenied
            ),
        }
        Ok(())
    }

    /// Record usage. Without final provider usage, tokens are estimated from bytes.
    async fn finish(
        &self,
        usage: Value,
        output_bytes: usize,
        final_usage: bool,
        status: &str,
        upstream_status: Option<i32>,
    ) -> anyhow::Result<()> {
        let input = usage["input_tokens"]
            .as_i64()
            .unwrap_or(self.input_bytes.div_ceil(4) as i64);
        let output = usage["output_tokens"]
            .as_i64()
            .unwrap_or(output_bytes.div_ceil(4) as i64);
        let raw = if usage.is_object() { usage } else { json!({}) };
        let mut tx = self.app.store.tx(self.key.org_id).await?;
        sqlx::query(
            "UPDATE usage_records SET input_tokens=$3,output_tokens=$4,raw_usage=$5,estimated=$6,status=$7,
                 upstream_status=$8,finished_at=now()
             WHERE org_id=$1 AND id=$2",
        )
        .bind(self.key.org_id)
        .bind(self.request_id)
        .bind(input)
        .bind(output)
        .bind(raw)
        .bind(!final_usage)
        .bind(status)
        .bind(upstream_status)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

/// `GET /orgs/{org}/usage`: the caller's records, or the organization's for owners/admins.
pub async fn usage(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<axum::Json<Vec<Value>>> {
    let ctx = crate::http::member(&app, &headers, org).await?;
    let all = authorize(
        &Principal::User(ctx.clone()),
        Action::ViewOrgUsage,
        &Resource {
            org_id: org,
            id: org,
            owner_user_id: None,
            target_role: None,
        },
    )
    .is_ok();
    let mut tx = app.store.tx(org).await?;
    let rows = sqlx::query_scalar(
        "SELECT to_jsonb(u) FROM usage_records u WHERE org_id=$1 AND ($3 OR user_id=$2)
         ORDER BY created_at DESC LIMIT 100",
    )
    .bind(org)
    .bind(ctx.user_id)
    .bind(all)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(axum::Json(rows))
}
