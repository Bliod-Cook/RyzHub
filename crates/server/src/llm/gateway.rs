//! Native-protocol model proxy. Requests pass through unchanged apart from authentication
//! (and the model id where an upstream names it differently); usage is read from the stream as
//! it is forwarded. Anthropic Messages, OpenAI Responses, OpenAI Chat Completions and Gemini
//! are spoken; each model's route says where it is served.
use anyhow::{Context, ensure};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, State},
    http::HeaderMap,
    response::Response,
    routing::{get, post},
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
    config::{LlmConfig, Upstream},
    error::{ApiResult, Code},
    provider::SseDecoder,
    store::{Tx, context_in, effective_policy},
};

/// Fallback re-validation of an accepted stream when no revocation notice arrives.
const STREAM_RECHECK: Duration = Duration::from_secs(5);
/// Model requests an organization may make per minute.
const REQUESTS_PER_MINUTE_ORG: i32 = 120;

pub fn router(app: App) -> Router {
    Router::new()
        .route("/mcp", post(crate::mcp::endpoint))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/responses", post(responses))
        .route("/v1/responses/compact", post(compact))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1beta/models/{target}", post(gemini))
        .route("/v1/models", get(list_models))
        .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024))
        .with_state(app)
}

pub fn bearer(headers: &HeaderMap) -> anyhow::Result<&str> {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .context(Code::Unauthorized)
}

/// Identity and liveness: the caller is still a member, the run is active, and the key's
/// binding (Owner epoch for loop keys, sandbox lease for sandbox keys) is current. Once a
/// stream has been accepted (`streaming`), expiry and supersession no longer interrupt it.
pub async fn validate_key(
    tx: &mut Tx<'_>,
    key: &GatewayKey,
    streaming: bool,
) -> anyhow::Result<()> {
    let ctx = context_in(tx, key.user_id, key.org_id).await?;
    let run = sqlx::query(
        "SELECT r.exec_epoch,r.state,c.creator_id,ch.writer_epoch,
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
    match key.kind() {
        KeyKind::Loop => {
            let epoch = key.exec_epoch;
            ensure!(
                epoch == Some(run.get("exec_epoch"))
                    && epoch == Some(run.get("writer_epoch"))
                    && run.get::<Option<bool>, _>("live") == Some(true),
                Code::Fenced
            );
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
        }
    }
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
    Ok(())
}

/// The model is allowed by current policy (and by the key, for a key's request), and so is the
/// reasoning level the call asks for.
async fn authorize_model(
    app: &App,
    tx: &mut Tx<'_>,
    org: Uuid,
    key: Option<&GatewayKey>,
    call: &Call,
) -> anyhow::Result<Policy> {
    let policy = effective_policy(tx, &app.config.policy, org).await?;
    ensure!(
        key.is_none_or(|key| key.models.contains(&call.model))
            && policy
                .models
                .as_ref()
                .is_none_or(|m| m.contains(&call.model)),
        Code::ModelDenied
    );
    if let (Some(limit), Some(asked)) = (
        &policy.max_reasoning,
        call.protocol.reasoning(&call.request),
    ) {
        ensure!(
            ryzhub_domain::reasoning_rank(&asked) <= ryzhub_domain::reasoning_rank(limit),
            Code::ReasoningDenied
        );
    }
    Ok(policy)
}

/// Fixed one-minute windows per organization and, for a key's request, per key (loop keys:
/// per run).
async fn rate_limit(
    app: &App,
    tx: &mut Tx<'_>,
    org: Uuid,
    key: Option<&GatewayKey>,
) -> anyhow::Result<()> {
    let per_key = key.map(|key| {
        let scope = match key.kind() {
            KeyKind::Loop => format!("run:{}", key.run_id),
            KeyKind::Sandbox => format!("key:{}", key.id),
        };
        (scope, app.config.llm.requests_per_minute_key)
    });
    for (scope, limit) in
        std::iter::once(("org".to_owned(), REQUESTS_PER_MINUTE_ORG)).chain(per_key)
    {
        let count: i32 = sqlx::query_scalar(
            "INSERT INTO model_rate_limits(org_id,scope,minute,requests)
             VALUES($1,$2,floor(extract(epoch FROM now())/60)::bigint,1)
             ON CONFLICT(org_id,scope,minute) DO UPDATE SET requests=model_rate_limits.requests+1
             RETURNING requests",
        )
        .bind(org)
        .bind(scope)
        .fetch_one(&mut **tx)
        .await?;
        ensure!(count <= limit, Code::RateLimited);
    }
    Ok(())
}

async fn messages(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let call = Call::of(Protocol::Messages, "/v1/messages", &body)?;
    Ok(proxy(app, headers, body, call).await?)
}

async fn responses(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let call = Call::of(Protocol::Responses, "/v1/responses", &body)?;
    Ok(proxy(app, headers, body, call).await?)
}

async fn compact(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let call = Call::of(Protocol::Responses, "/v1/responses/compact", &body)?;
    Ok(proxy(app, headers, body, call).await?)
}

async fn chat_completions(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let mut call = Call::of(Protocol::ChatCompletions, "/v1/chat/completions", &body)?;
    // Usage arrives in a stream only when asked for.
    let body = if call.streaming {
        let mut request = call.request.clone();
        request["stream_options"]["include_usage"] = json!(true);
        call.request = request;
        Bytes::from(serde_json::to_vec(&call.request)?)
    } else {
        body
    };
    Ok(proxy(app, headers, body, call).await?)
}

/// `POST /v1beta/models/{model}:generateContent` and `:streamGenerateContent?alt=sse`.
async fn gemini(
    State(app): State<App>,
    headers: HeaderMap,
    Path(target): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let (model, action) = target.rsplit_once(':').context(Code::InvalidInput)?;
    let streaming = match action {
        "generateContent" => false,
        "streamGenerateContent" => true,
        _ => return Err(Code::InvalidInput.into()),
    };
    let request: Value = serde_json::from_slice(&body).map_err(|_| Code::InvalidInput)?;
    let call = Call {
        protocol: Protocol::Gemini,
        // Streams come as server-sent events.
        path: format!(
            "/v1beta/models/{model}:{action}{}",
            if streaming { "?alt=sse" } else { "" }
        ),
        model: model.to_owned(),
        streaming,
        request,
        count_only: false,
    };
    Ok(proxy(app, headers, body, call).await?)
}

/// `POST /v1/messages/count_tokens`: answered by the upstream, no usage recorded.
async fn count_tokens(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let mut call = Call::of(Protocol::Messages, "/v1/messages/count_tokens", &body)?;
    call.count_only = true;
    call.streaming = false;
    Ok(proxy(app, headers, body, call).await?)
}

/// `GET /v1/models`: the models this key may use, in the caller's format (Anthropic's when it
/// sends `anthropic-version`, else OpenAI's).
async fn list_models(State(app): State<App>, headers: HeaderMap) -> ApiResult<axum::Json<Value>> {
    let key = keys::resolve(&app.store, bearer(&headers)?).await?;
    let mut tx = app.store.tx(key.org_id).await?;
    validate_key(&mut tx, &key, false).await?;
    let policy = effective_policy(&mut tx, &app.config.policy, key.org_id).await?;
    tx.commit().await?;
    let models: Vec<&String> = key
        .models
        .iter()
        .filter(|m| policy.models.as_ref().is_none_or(|p| p.contains(*m)))
        .collect();
    Ok(axum::Json(if headers.contains_key("anthropic-version") {
        json!({
            "data": models.iter().map(|id| json!({"type":"model","id":id,"display_name":id,"created_at":"1970-01-01T00:00:00Z"})).collect::<Vec<_>>(),
            "has_more": false,
            "first_id": models.first(),
            "last_id": models.last(),
        })
    } else {
        json!({
            "object": "list",
            "data": models.iter().map(|id| json!({"id":id,"object":"model","created":0,"owned_by":"ryzhub"})).collect::<Vec<_>>(),
        })
    }))
}

/// A model protocol the gateway speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Protocol {
    Messages,
    Responses,
    ChatCompletions,
    Gemini,
}

impl Protocol {
    /// The kind of member credential it takes.
    fn credential(self) -> &'static str {
        match self {
            Self::Messages => "anthropic",
            Self::Responses | Self::ChatCompletions => "openai",
            Self::Gemini => "gemini",
        }
    }

    /// Its name in `llm.models` routes.
    fn route(self) -> &'static str {
        match self {
            Self::Messages => "anthropic",
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
            Self::Gemini => "gemini",
        }
    }

    /// Where it is served when a route names no upstream, and the variable of its key.
    fn default_upstream(self, llm: &LlmConfig) -> (&str, &str) {
        match self {
            Self::Messages => (&llm.upstream_url, &llm.platform_key_env),
            Self::Responses => (&llm.responses_url, &llm.responses_key_env),
            Self::ChatCompletions => (&llm.chat_completions_url, &llm.chat_completions_key_env),
            Self::Gemini => (&llm.gemini_url, &llm.gemini_key_env),
        }
    }

    /// The request's reasoning level, when it asks for one, ranked like policy levels.
    fn reasoning(self, request: &Value) -> Option<String> {
        // Thinking budgets in tokens, as the least level that allows them.
        let budget = |tokens: Option<i64>| {
            tokens.filter(|t| *t > 0).map(|t| {
                match t {
                    ..=1024 => "minimal",
                    1025..=4096 => "low",
                    4097..=16384 => "medium",
                    16385..=32768 => "high",
                    32769..=65536 => "xhigh",
                    _ => "max",
                }
                .to_owned()
            })
        };
        // `none` asks for no reasoning at all.
        let named = |value: &Value| {
            value
                .as_str()
                .filter(|level| *level != "none")
                .map(str::to_owned)
        };
        match self {
            Self::Messages => named(&request["output_config"]["effort"])
                .or_else(|| budget(request["thinking"]["budget_tokens"].as_i64())),
            Self::Responses => named(&request["reasoning"]["effort"]),
            Self::ChatCompletions => named(&request["reasoning_effort"]),
            Self::Gemini => {
                let thinking = &request["generationConfig"]["thinkingConfig"];
                named(&thinking["thinkingLevel"])
                    .map(|l| l.to_lowercase())
                    .or_else(|| budget(thinking["thinkingBudget"].as_i64()))
            }
        }
    }
}

/// One request to forward.
struct Call {
    protocol: Protocol,
    /// The upstream path, with its query.
    path: String,
    model: String,
    streaming: bool,
    request: Value,
    /// Answered without recording usage (token counting).
    count_only: bool,
}

impl Call {
    fn of(protocol: Protocol, path: &str, body: &[u8]) -> anyhow::Result<Self> {
        let request: Value = serde_json::from_slice(body).map_err(|_| Code::InvalidInput)?;
        let model = request["model"]
            .as_str()
            .context(Code::InvalidInput)?
            .to_owned();
        Ok(Self {
            protocol,
            path: path.to_owned(),
            model,
            streaming: request["stream"].as_bool().unwrap_or(false),
            request,
            count_only: false,
        })
    }
}

/// Where a request may go: an upstream, the key it takes there and the model's id there.
struct Target {
    url: String,
    secret: String,
    model: Option<String>,
}

/// The upstreams `call` may go to, in the order to try them, and the member credential used.
/// A member's own credential goes to the protocol's default upstream alone: their key is not
/// good elsewhere.
async fn targets(
    app: &App,
    tx: &mut Tx<'_>,
    (org, user): (Uuid, Uuid),
    policy: &Policy,
    call: &Call,
) -> anyhow::Result<(Option<Uuid>, Vec<Target>)> {
    let llm = &app.config.llm;
    let (default_url, default_env) = call.protocol.default_upstream(llm);
    let credential = call.protocol.credential();
    if let Some((id, secret)) =
        credentials::member_key(app, tx, org, user, policy, credential).await?
    {
        let target = Target {
            url: default_url.to_owned(),
            secret,
            model: None,
        };
        return Ok((Some(id), vec![target]));
    }
    ensure!(
        policy.allow_platform_credentials != Some(false),
        Code::PlatformCredentialDenied
    );
    let route = llm
        .models
        .iter()
        .find(|r| r.id == call.model && r.protocol == call.protocol.route());
    let configured = route.map(|r| r.upstreams.clone()).unwrap_or_default();
    let upstreams = if configured.is_empty() {
        vec![Upstream {
            url: default_url.to_owned(),
            ..Default::default()
        }]
    } else {
        configured
    };
    let targets: Vec<Target> = order(upstreams, rand::random)
        .into_iter()
        .filter_map(|u| {
            let env = if u.key_env.is_empty() {
                default_env
            } else {
                &u.key_env
            };
            Some(Target {
                secret: std::env::var(env).ok()?,
                model: (!u.model.is_empty()).then(|| u.model.clone()),
                url: u.url,
            })
        })
        .collect();
    ensure!(!targets.is_empty(), Code::CredentialUnavailable);
    Ok((None, targets))
}

/// Upstreams in the order to try them: by priority, and within one at random, each in
/// proportion to its weight.
fn order(mut upstreams: Vec<Upstream>, mut random: impl FnMut() -> u64) -> Vec<Upstream> {
    upstreams.sort_by_key(|u| u.priority);
    let mut ordered = Vec::with_capacity(upstreams.len());
    while !upstreams.is_empty() {
        let priority = upstreams[0].priority;
        let tier = upstreams
            .iter()
            .take_while(|u| u.priority == priority)
            .count();
        let total: u64 = upstreams[..tier].iter().map(|u| u.weight as u64).sum();
        let mut pick = random() % total;
        let index = upstreams[..tier]
            .iter()
            .position(|u| {
                let weight = u.weight as u64;
                if pick < weight {
                    true
                } else {
                    pick -= weight;
                    false
                }
            })
            .expect("a pick below the tier's total weight");
        ordered.push(upstreams.remove(index));
    }
    ordered
}

/// Headers forwarded to the provider unchanged.
const FORWARDED: [&str; 5] = [
    "anthropic-version",
    "anthropic-beta",
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
];

async fn proxy(app: App, headers: HeaderMap, body: Bytes, call: Call) -> anyhow::Result<Response> {
    ensure!(app.config.llm.enabled, Code::ModelGatewayDisabled);
    let key = keys::resolve(&app.store, bearer(&headers)?).await?;
    let mut tx = app.store.tx(key.org_id).await?;
    validate_key(&mut tx, &key, false).await?;
    let policy = authorize_model(&app, &mut tx, key.org_id, Some(&key), &call).await?;
    let (credential, targets) =
        targets(&app, &mut tx, (key.org_id, key.user_id), &policy, &call).await?;
    let ledger = Ledger {
        app: app.clone(),
        org: key.org_id,
        request_id: Uuid::now_v7(),
        protocol: call.protocol,
        input_bytes: body.len(),
    };
    if !call.count_only {
        rate_limit(&app, &mut tx, key.org_id, Some(&key)).await?;
        ledger
            .start(
                &mut tx,
                key.user_id,
                Some(&key),
                &headers,
                &call,
                credential,
            )
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
            &ledger.request_id.to_string(),
            "accepted",
        )
        .await?;
    }
    tx.commit().await?;

    let Some(upstream) = send(&app, &headers, &call, body, targets).await else {
        if !call.count_only {
            ledger
                .finish(Value::Null, 0, false, "upstream_unreachable", None)
                .await?;
        }
        anyhow::bail!(Code::UpstreamUnavailable);
    };
    let meter = Meter {
        ledger,
        key,
        model: call.model.clone(),
        credential,
        streaming: call.streaming,
        count_only: call.count_only,
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

/// Each upstream in turn until one answers with something other than an outage.
async fn send(
    app: &App,
    headers: &HeaderMap,
    call: &Call,
    body: Bytes,
    targets: Vec<Target>,
) -> Option<reqwest::Response> {
    let last = targets.len() - 1;
    for (n, target) in targets.into_iter().enumerate() {
        let body = match &target.model {
            Some(model) if call.protocol != Protocol::Gemini => {
                let mut request = call.request.clone();
                request["model"] = json!(model);
                Bytes::from(serde_json::to_vec(&request).ok()?)
            }
            _ => body.clone(),
        };
        let path = match (&target.model, call.protocol) {
            (Some(model), Protocol::Gemini) => call.path.replacen(&call.model, model, 1),
            _ => call.path.clone(),
        };
        let mut outgoing = app
            .http
            .upstream
            .post(format!("{}{path}", target.url.trim_end_matches('/')))
            .header("content-type", "application/json")
            .body(body);
        outgoing = match call.protocol {
            Protocol::Responses | Protocol::ChatCompletions => outgoing.bearer_auth(&target.secret),
            Protocol::Messages => outgoing.header("x-api-key", &target.secret),
            Protocol::Gemini => outgoing.header("x-goog-api-key", &target.secret),
        };
        for name in FORWARDED {
            if let Some(value) = headers.get(name) {
                outgoing = outgoing.header(name, value);
            }
        }
        if call.protocol == Protocol::Messages && !headers.contains_key("anthropic-version") {
            outgoing = outgoing.header("anthropic-version", "2023-06-01");
        }
        match outgoing.send().await {
            Ok(response)
                if n < last
                    && (response.status().is_server_error()
                        || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS) =>
            {
                tracing::info!(status = %response.status(), url = %target.url, "upstream refused; trying the next");
            }
            Ok(response) => return Some(response),
            Err(error) if n < last => {
                tracing::info!(%error, url = %target.url, "upstream unreachable; trying the next");
            }
            Err(_) => {}
        }
    }
    None
}

/// A model call the server makes for `user` outside any run (history digests): held to the
/// same model policy, routes, credentials, organization rate limit and usage ledger as a key's
/// request. `protocol` is `responses` or `anthropic`; the answer comes at once, as JSON.
pub async fn complete(
    app: &App,
    org: Uuid,
    user: Uuid,
    protocol: &str,
    request: &Value,
) -> anyhow::Result<Value> {
    ensure!(app.config.llm.enabled, Code::ModelGatewayDisabled);
    let (protocol, path) = match protocol {
        "responses" => (Protocol::Responses, "/v1/responses"),
        _ => (Protocol::Messages, "/v1/messages"),
    };
    let body = Bytes::from(serde_json::to_vec(request)?);
    let call = Call::of(protocol, path, &body)?;
    ensure!(!call.streaming, Code::InvalidInput);
    let mut tx = app.store.tx(org).await?;
    let ctx = context_in(&mut tx, user, org).await?;
    let resource = Resource {
        org_id: org,
        id: org,
        owner_user_id: Some(user),
        target_role: None,
    };
    authorize(&Principal::User(ctx), Action::UseModel, &resource).map_err(|_| Code::Forbidden)?;
    let policy = authorize_model(app, &mut tx, org, None, &call).await?;
    let (credential, targets) = targets(app, &mut tx, (org, user), &policy, &call).await?;
    rate_limit(app, &mut tx, org, None).await?;
    let ledger = Ledger {
        app: app.clone(),
        org,
        request_id: Uuid::now_v7(),
        protocol,
        input_bytes: body.len(),
    };
    let headers = HeaderMap::new();
    ledger
        .start(&mut tx, user, None, &headers, &call, credential)
        .await?;
    tx.commit().await?;
    let Some(upstream) = send(app, &headers, &call, body, targets).await else {
        ledger
            .finish(Value::Null, 0, false, "upstream_unreachable", None)
            .await?;
        anyhow::bail!(Code::UpstreamUnavailable);
    };
    let status = upstream.status();
    let read = async {
        let mut stream = upstream.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?);
            ensure!(bytes.len() <= 4 * 1024 * 1024, Code::ContentTooLarge);
        }
        Ok::<_, anyhow::Error>(bytes)
    }
    .await;
    let value: Value = read
        .as_deref()
        .ok()
        .and_then(|bytes| serde_json::from_slice(bytes).ok())
        .unwrap_or_default();
    let usage = value["usage"].clone();
    let final_usage = usage.is_object();
    let outcome = match (status.is_success(), final_usage) {
        (false, _) => "upstream_error",
        (true, true) => "completed",
        (true, false) => "interrupted",
    };
    let output = read.as_ref().map_or(0, Vec::len);
    let upstream_status = Some(status.as_u16() as i32);
    ledger
        .finish(usage, output, final_usage, outcome, upstream_status)
        .await?;
    read?;
    ensure!(status.is_success(), Code::ModelRequestFailed);
    Ok(value)
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

/// One request's usage record: started when the request is accepted, finished when it ends.
struct Ledger {
    app: App,
    org: Uuid,
    request_id: Uuid,
    protocol: Protocol,
    input_bytes: usize,
}

/// Forwards one upstream response, re-validates the caller while it streams, and records usage.
struct Meter {
    ledger: Ledger,
    key: GatewayKey,
    model: String,
    credential: Option<Uuid>,
    streaming: bool,
    /// Token counting: nothing to record.
    count_only: bool,
}

/// Usage as a stream reports it, observed without keeping the content.
#[derive(Default)]
struct StreamUsage {
    usage: Value,
    /// The stream reached its end.
    finished: bool,
    /// It ended as a failure.
    failed: bool,
}

impl StreamUsage {
    fn observe(&mut self, protocol: Protocol, event: &Value) {
        match protocol {
            // Usage starts in `message_start`; each `message_delta` updates some of its counts.
            Protocol::Messages => match event["type"].as_str() {
                Some("message_start") => self.usage = event["message"]["usage"].clone(),
                Some("message_delta") => {
                    if let Some(counts) = event["usage"].as_object() {
                        if !self.usage.is_object() {
                            self.usage = json!({});
                        }
                        for (name, count) in counts {
                            self.usage[name] = count.clone();
                        }
                    }
                }
                Some("message_stop") => self.finished = true,
                _ => {}
            },
            Protocol::Responses => {
                let kind = event["type"].as_str().unwrap_or("");
                if matches!(
                    kind,
                    "response.completed" | "response.incomplete" | "response.failed"
                ) {
                    self.usage = event["response"]["usage"].clone();
                    self.finished = true;
                    self.failed = kind != "response.completed";
                }
            }
            // The last chunk carries the usage the request asked for.
            Protocol::ChatCompletions => {
                if event["usage"].is_object() {
                    self.usage = event["usage"].clone();
                    self.finished = true;
                }
            }
            // Every chunk carries the usage so far.
            Protocol::Gemini => {
                if event["usageMetadata"].is_object() {
                    self.usage = event["usageMetadata"].clone();
                }
                if event["candidates"][0]["finishReason"].is_string() {
                    self.finished = true;
                }
            }
        }
    }
}

/// `usage` as input and output tokens, whatever the protocol calls them. Gemini counts thoughts
/// apart from the answer; both are output.
fn tokens(protocol: Protocol, usage: &Value) -> (Option<i64>, Option<i64>) {
    match protocol {
        // A compaction is billed apart from the message: each is an iteration, and the
        // top-level counts leave the compaction out.
        Protocol::Messages if usage["iterations"].is_array() => {
            let sum = |field: &str| {
                usage["iterations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|i| i[field].as_i64())
                    .sum::<Option<i64>>()
            };
            (sum("input_tokens"), sum("output_tokens"))
        }
        Protocol::Messages | Protocol::Responses => (
            usage["input_tokens"].as_i64(),
            usage["output_tokens"].as_i64(),
        ),
        Protocol::ChatCompletions => (
            usage["prompt_tokens"].as_i64(),
            usage["completion_tokens"].as_i64(),
        ),
        Protocol::Gemini => (
            usage["promptTokenCount"].as_i64(),
            usage["candidatesTokenCount"]
                .as_i64()
                .map(|candidates| candidates + usage["thoughtsTokenCount"].as_i64().unwrap_or(0)),
        ),
    }
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
        let mut parsed = StreamUsage::default();
        let mut notices = self.ledger.app.revocations.subscribe();
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
                    match decoder.push(&bytes) {
                        Ok(events) => {
                            for event in &events {
                                parsed.observe(self.ledger.protocol, event);
                            }
                        }
                        Err(_) => parse_ok = false,
                    }
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
        if self.count_only {
            return;
        }
        let (usage, finished) = if self.streaming {
            (parsed.usage, parsed.finished)
        } else {
            let value: Value = serde_json::from_slice(&json_body).unwrap_or(Value::Null);
            let usage = if self.ledger.protocol == Protocol::Gemini {
                value["usageMetadata"].clone()
            } else {
                value["usage"].clone()
            };
            (usage, true)
        };
        let final_usage = completed && parse_ok && finished && usage.is_object();
        let outcome = if !status.is_success() || parsed.failed {
            "upstream_error"
        } else if completed && final_usage {
            "completed"
        } else {
            "interrupted"
        };
        let upstream_status = Some(status.as_u16() as i32);
        let finished = self
            .ledger
            .finish(usage, received, final_usage, outcome, upstream_status)
            .await;
        if let Err(error) = finished {
            let request_id = self.ledger.request_id;
            tracing::error!(%request_id, %error, "usage finalization failed");
        }
        // The response closes only after accounting; a disconnected client still leaves an
        // estimated record.
        drop(sender);
    }

    /// Authorization of an accepted stream: key binding, model policy and credential.
    async fn revalidate(&self) -> anyhow::Result<()> {
        let mut tx = self.ledger.app.store.tx(self.key.org_id).await?;
        let result = self.authorized(&mut tx).await;
        // Read-only: end it explicitly so the connection is back in the pool at once.
        tx.rollback().await?;
        result
    }

    async fn authorized(&self, tx: &mut Tx<'_>) -> anyhow::Result<()> {
        validate_key(tx, &self.key, true).await?;
        let policy = effective_policy(tx, &self.ledger.app.config.policy, self.key.org_id).await?;
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
}

impl Ledger {
    /// Record the accepted request in `tx`, with the key and Claude Code's session metadata
    /// when it came with them.
    async fn start(
        &self,
        tx: &mut Tx<'_>,
        user: Uuid,
        key: Option<&GatewayKey>,
        headers: &HeaderMap,
        call: &Call,
        credential: Option<Uuid>,
    ) -> anyhow::Result<()> {
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
        .bind(self.request_id)
        .bind(self.org)
        .bind(user)
        .bind(key.map(|key| key.run_id))
        .bind(key.filter(|key| key.kind() == KeyKind::Sandbox).map(|key| key.id))
        .bind(&call.model)
        .bind(credential)
        .bind(metadata("x-claude-code-session-id"))
        .bind(metadata("x-claude-code-agent-id"))
        .bind(metadata("x-claude-code-parent-agent-id"))
        .bind(self.protocol.credential())
        .execute(&mut **tx)
        .await?;
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
        let (input, output) = tokens(self.protocol, &usage);
        let input = input.unwrap_or(self.input_bytes.div_ceil(4) as i64);
        let output = output.unwrap_or(output_bytes.div_ceil(4) as i64);
        let raw = if usage.is_object() { usage } else { json!({}) };
        let mut tx = self.app.store.tx(self.org).await?;
        sqlx::query(
            "UPDATE usage_records SET input_tokens=$3,output_tokens=$4,raw_usage=$5,estimated=$6,status=$7,
                 upstream_status=$8,finished_at=now()
             WHERE org_id=$1 AND id=$2",
        )
        .bind(self.org)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(url: &str, priority: u32, weight: u32) -> Upstream {
        Upstream {
            url: url.into(),
            priority,
            weight,
            ..Default::default()
        }
    }

    #[test]
    fn upstreams_go_by_priority_then_weight() {
        let upstreams = vec![
            upstream("c", 1, 1),
            upstream("a", 0, 3),
            upstream("b", 0, 1),
        ];
        let urls = |random: u64| -> Vec<String> {
            order(upstreams.clone(), || random)
                .into_iter()
                .map(|u| u.url)
                .collect()
        };
        // Within priority 0, picks below 3 land on `a`, the fourth on `b`.
        assert_eq!(urls(0), ["a", "b", "c"]);
        assert_eq!(urls(2), ["a", "b", "c"]);
        assert_eq!(urls(3), ["b", "a", "c"]);
        let mut first = std::collections::BTreeMap::<String, usize>::new();
        let mut seed = 7u64;
        for _ in 0..4000 {
            let ordered = order(upstreams.clone(), || {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                seed >> 33
            });
            assert_eq!(ordered[2].url, "c");
            *first.entry(ordered[0].url.clone()).or_default() += 1;
        }
        assert!((2700..3300).contains(&first["a"]), "{first:?}");
    }

    #[test]
    fn reasoning_reads_each_protocol() {
        let level = |protocol: Protocol, request: Value| protocol.reasoning(&request);
        assert_eq!(
            level(
                Protocol::Messages,
                json!({"output_config":{"effort":"high"}})
            )
            .as_deref(),
            Some("high")
        );
        assert_eq!(
            level(
                Protocol::Messages,
                json!({"thinking":{"type":"enabled","budget_tokens":2048}})
            )
            .as_deref(),
            Some("low")
        );
        assert_eq!(level(Protocol::Messages, json!({})), None);
        assert_eq!(
            level(Protocol::Responses, json!({"reasoning":{"effort":"xhigh"}})).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            level(
                Protocol::ChatCompletions,
                json!({"reasoning_effort":"none"})
            ),
            None
        );
        assert_eq!(
            level(
                Protocol::Gemini,
                json!({"generationConfig":{"thinkingConfig":{"thinkingLevel":"HIGH"}}})
            )
            .as_deref(),
            Some("high")
        );
        assert_eq!(
            level(
                Protocol::Gemini,
                json!({"generationConfig":{"thinkingConfig":{"thinkingBudget":100000}}})
            )
            .as_deref(),
            Some("max")
        );
        // Off, and the model's own choice, ask for no level.
        for budget in [0, -1] {
            assert_eq!(
                level(
                    Protocol::Gemini,
                    json!({"generationConfig":{"thinkingConfig":{"thinkingBudget":budget}}})
                ),
                None
            );
        }
    }

    #[test]
    fn terminal_usage_does_not_turn_failure_into_success() {
        for kind in [
            "response.completed",
            "response.incomplete",
            "response.failed",
        ] {
            let mut usage = StreamUsage::default();
            let terminal =
                json!({"type":kind,"response":{"usage":{"input_tokens":10,"output_tokens":3}}});
            usage.observe(Protocol::Responses, &terminal);
            assert!(usage.finished && usage.usage.is_object());
            assert_eq!(usage.failed, kind != "response.completed");
        }
    }

    #[test]
    fn usage_reads_as_input_and_output_tokens() {
        assert_eq!(
            tokens(
                Protocol::ChatCompletions,
                &json!({"prompt_tokens":11,"completion_tokens":3})
            ),
            (Some(11), Some(3))
        );
        assert_eq!(
            tokens(
                Protocol::Gemini,
                &json!({"promptTokenCount":20,"candidatesTokenCount":2,"thoughtsTokenCount":5})
            ),
            (Some(20), Some(7))
        );
        assert_eq!(
            tokens(
                Protocol::Messages,
                &json!({"input_tokens":25,"output_tokens":8})
            ),
            (Some(25), Some(8))
        );
        let compacted = json!({"input_tokens":0,"output_tokens":0,"iterations":[
            {"type":"compaction","input_tokens":180000,"output_tokens":3000},
            {"type":"message","input_tokens":4000,"output_tokens":200}]});
        assert_eq!(
            tokens(Protocol::Messages, &compacted),
            (Some(184000), Some(3200))
        );
    }
}
