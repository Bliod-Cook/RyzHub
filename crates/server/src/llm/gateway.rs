use super::{
    credentials,
    identity::{self, Claims},
};
use crate::{
    api::{ApiError, ApiResult, App},
    provider::{NativeMessage, SseDecoder},
    store::{Tx, context_in},
};
use anyhow::{Context, ensure};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use futures_util::StreamExt;
use ryzhub_domain::{Action, Policy, Principal, Resource, authorize};
use serde_json::{Value, json};
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

pub fn router(app: App) -> Router {
    Router::new()
        .route("/mcp", post(crate::mcp::endpoint))
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .route("/v1/responses/compact", post(compact))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(app)
}
pub async fn effective_policy(app: &App, tx: &mut Tx<'_>, org: Uuid) -> anyhow::Result<Policy> {
    let policy: Option<Value> = sqlx::query_scalar(
        "SELECT policy FROM org_policies WHERE org_id=$1 ORDER BY version DESC LIMIT 1",
    )
    .bind(org)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(app
        .config
        .policy
        .restrict(&serde_json::from_value(policy.unwrap_or(json!({})))?))
}
/// Both entry identities are revalidated against current membership and execution state.
pub async fn validate(
    app: &App,
    tx: &mut Tx<'_>,
    claims: &Claims,
    model: &str,
) -> anyhow::Result<Option<Uuid>> {
    let ctx = context_in(tx, claims.user_id, claims.org_id).await?;
    let run=sqlx::query("SELECT r.exec_epoch,r.state,r.credential_id,c.creator_id,c.owner_epoch,ch.lease_until>clock_timestamp() AS live FROM runs r JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id JOIN channels ch ON ch.org_id=c.org_id AND ch.conversation_id=c.id WHERE r.org_id=$1 AND r.id=$2 AND r.conversation_id=$3 AND c.state='active'").bind(claims.org_id).bind(claims.run_id).bind(claims.conversation_id).fetch_optional(&mut **tx).await?.context("FORBIDDEN")?;
    ensure!(
        run.get::<Uuid, _>("creator_id") == claims.user_id
            && matches!(run.get::<&str, _>("state"), "running" | "requires_action"),
        "FORBIDDEN"
    );
    let resource = Resource {
        org_id: claims.org_id,
        id: claims.conversation_id,
        owner_user_id: Some(claims.user_id),
        target_role: None,
    };
    let explicit = match claims.kind.as_str() {
        "loop" => {
            ensure!(
                claims.exec_epoch == Some(run.get("exec_epoch"))
                    && claims.exec_epoch == Some(run.get("owner_epoch"))
                    && run.get::<Option<bool>, _>("live") == Some(true),
                "FENCED"
            );
            authorize(
                &Principal::Service {
                    org_id: claims.org_id,
                    actions: std::collections::BTreeSet::from([Action::UseModel]),
                },
                Action::UseModel,
                &resource,
            )?;
            run.get("credential_id")
        }
        "sandbox" => {
            let row=sqlx::query("SELECT v.models,v.credential_id FROM virtual_keys v JOIN sandboxes s ON s.org_id=v.org_id AND s.id=v.sandbox_id WHERE v.org_id=$1 AND v.id=$2 AND v.user_id=$3 AND v.run_id=$4 AND v.sandbox_id=$5 AND v.lease_epoch=$6 AND s.lease_epoch=v.lease_epoch AND s.state='running' AND v.revoked_at IS NULL AND v.superseded_at IS NULL AND v.expires_at>now() AND (s.node_lease_until IS NULL OR s.node_lease_until>clock_timestamp())").bind(claims.org_id).bind(claims.jti).bind(claims.user_id).bind(claims.run_id).bind(claims.sandbox_id).bind(claims.lease_epoch).fetch_optional(&mut **tx).await?.context("UNAUTHORIZED")?;
            ensure!(
                row.get::<Vec<String>, _>("models")
                    .iter()
                    .any(|m| m == model),
                "MODEL_DENIED"
            );
            authorize(
                &Principal::VirtualKey {
                    org_id: claims.org_id,
                    user_id: claims.user_id,
                    resource_id: claims.conversation_id,
                    revoked: false,
                },
                Action::UseModel,
                &resource,
            )?;
            row.get("credential_id")
        }
        _ => anyhow::bail!("UNAUTHORIZED"),
    };
    authorize(&Principal::User(ctx), Action::UseModel, &resource)?;
    let policy = effective_policy(app, tx, claims.org_id).await?;
    ensure!(
        claims.models.iter().any(|m| m == model)
            && policy.models.as_ref().is_none_or(|m| m.contains(model)),
        "MODEL_DENIED"
    );
    Ok(explicit)
}
async fn rate_limit(app: &App, tx: &mut Tx<'_>, claims: &Claims) -> anyhow::Result<()> {
    // Loop tokens rotate on each attempt, so their rate scope is the stable run id.
    let scopes = [
        ("org".to_owned(), app.config.llm.requests_per_minute_org),
        (
            if claims.kind == "loop" {
                format!("run:{}", claims.run_id)
            } else {
                format!("key:{}", claims.jti)
            },
            app.config.llm.requests_per_minute_key,
        ),
    ];
    for (scope, limit) in scopes {
        let count:i32=sqlx::query_scalar("INSERT INTO model_rate_limits(org_id,scope,minute,requests) VALUES($1,$2,floor(extract(epoch FROM now())/60)::bigint,1) ON CONFLICT(org_id,scope,minute) DO UPDATE SET requests=model_rate_limits.requests+1 RETURNING requests").bind(claims.org_id).bind(scope).fetch_one(&mut **tx).await?;
        ensure!(count <= limit, "RATE_LIMITED");
    }
    Ok(())
}
async fn messages(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    proxy(app, headers, body, "anthropic", "/v1/messages").await
}
async fn responses(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    proxy(app, headers, body, "openai", "/v1/responses").await
}
async fn compact(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    proxy(app, headers, body, "openai", "/v1/responses/compact").await
}
async fn proxy(
    app: App,
    headers: HeaderMap,
    body: Bytes,
    provider: &'static str,
    path: &str,
) -> ApiResult<Response> {
    crate::api::check(app.config.llm.enabled, "MODEL_GATEWAY_DISABLED")?;
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .context("UNAUTHORIZED")?;
    let claims =
        identity::verify(&app.config.llm, token).map_err(|_| anyhow::anyhow!("UNAUTHORIZED"))?;
    let request: Value =
        serde_json::from_slice(&body).map_err(|_| anyhow::anyhow!("INVALID_INPUT"))?;
    let model = request["model"]
        .as_str()
        .context("INVALID_INPUT")?
        .to_owned();
    let streaming = request["stream"].as_bool().unwrap_or(false);
    let mut tx = app.store.tx(claims.org_id).await?;
    let explicit = validate(&app, &mut tx, &claims, &model).await?;
    let policy = effective_policy(&app, &mut tx, claims.org_id).await?;
    let (credential, key) = credentials::resolve(
        &app,
        &mut tx,
        claims.org_id,
        claims.user_id,
        explicit,
        &policy,
        provider,
    )
    .await?;
    rate_limit(&app, &mut tx, &claims).await?;
    let request_id = Uuid::now_v7();
    let metadata = |name: &str| {
        headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .map(|h| h.chars().take(256).collect::<String>())
    };
    sqlx::query("INSERT INTO usage_records(id,org_id,user_id,run_id,virtual_key_id,model,credential_id,session_id,agent_id,parent_agent_id,provider) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)").bind(request_id).bind(claims.org_id).bind(claims.user_id).bind(claims.run_id).bind((claims.kind=="sandbox").then_some(claims.jti)).bind(&model).bind(credential).bind(metadata("x-claude-code-session-id")).bind(metadata("x-claude-code-agent-id")).bind(metadata("x-claude-code-parent-agent-id")).bind(provider).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO audit_logs(id,org_id,actor_id,actor_kind,action,resource,result) VALUES($1,$2,$3,$4,'model.request',$5,'accepted')").bind(Uuid::now_v7()).bind(claims.org_id).bind(claims.user_id).bind(&claims.kind).bind(request_id.to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(app.config.llm.timeout_secs))
        .build()?;
    let mut outgoing = client
        .post(format!(
            "{}{}",
            (if provider == "openai" {
                &app.config.llm.responses_url
            } else {
                &app.config.llm.upstream_url
            })
            .trim_end_matches('/'),
            path
        ))
        .header("content-type", "application/json")
        .body(body.clone());
    outgoing = if provider == "openai" {
        outgoing.bearer_auth(key)
    } else {
        outgoing.header("x-api-key", key)
    };
    for name in [
        "anthropic-version",
        "anthropic-beta",
        "x-claude-code-session-id",
        "x-claude-code-agent-id",
        "x-claude-code-parent-agent-id",
    ] {
        if let Some(value) = headers.get(name) {
            outgoing = outgoing.header(name, value);
        }
    }
    if provider == "anthropic" && !headers.contains_key("anthropic-version") {
        outgoing = outgoing.header("anthropic-version", "2023-06-01");
    }
    let upstream = match outgoing.send().await {
        Ok(response) => response,
        Err(_) => {
            finish_usage(
                &app,
                &claims,
                request_id,
                Value::Null,
                0,
                body.len(),
                false,
                "upstream_unreachable",
                None,
            )
            .await?;
            return Err(ApiError(anyhow::anyhow!("UPSTREAM_UNAVAILABLE")));
        }
    };
    let status = upstream.status();
    let upstream_status = Some(status.as_u16() as i32);
    let mut response = Response::builder().status(status);
    for name in ["content-type", "request-id", "x-request-id"] {
        if let Some(value) = upstream.headers().get(name) {
            response = response.header(name, value);
        }
    }
    response = response
        .header("cache-control", "no-store")
        .header("x-accel-buffering", "no");
    if let Some(retry) = upstream
        .headers()
        .get("retry-after")
        .and_then(|h| h.to_str().ok())
    {
        let seconds = retry
            .parse::<u64>()
            .ok()
            .or_else(|| {
                chrono::DateTime::parse_from_rfc2822(retry)
                    .ok()
                    .map(|t| (t.timestamp() - chrono::Utc::now().timestamp()).max(0) as u64)
            })
            .unwrap_or(1)
            .clamp(1, 60);
        response = response.header("retry-after", seconds.to_string());
    }
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    tokio::spawn(async move {
        let mut stream = upstream.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut native = NativeMessage::default();
        let mut responses = crate::provider::ResponsesUsage::default();
        let mut recheck = tokio::time::interval(Duration::from_millis(250));
        let mut parse_ok = true;
        let mut completed = true;
        let mut received = 0usize;
        let mut json_body = Vec::new();
        loop {
            let next = tokio::select! {
                _=sender.closed()=>{completed=false;break;},
                _=recheck.tick()=>{
                    if validate_stream(&app, &claims, &model, credential).await.is_err() {
                        completed=false;
                        let _=sender.try_send(Err(std::io::Error::other("authorization revoked")));
                        break;
                    }
                    continue;
                },
                chunk=stream.next()=>chunk
            };
            let Some(chunk) = next else {
                break;
            };
            match chunk {
                Ok(bytes) => {
                    received += bytes.len();
                    if streaming && status.is_success() {
                        if parse_ok {
                            match decoder.push(&bytes) {
                                Ok(events) => {
                                    for event in events {
                                        if (if provider == "openai" {
                                            responses.apply(&event)
                                        } else {
                                            native.apply(&event).map(|_| ())
                                        })
                                        .is_err()
                                        {
                                            parse_ok = false;
                                            break;
                                        }
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
                Err(_) => {
                    completed = false;
                    let _ = sender
                        .send(Err(std::io::Error::other("upstream stream interrupted")))
                        .await;
                    break;
                }
            }
        }
        let (usage, final_usage, output_bytes) = if streaming && provider == "openai" {
            (
                responses.usage,
                completed && parse_ok && responses.finished,
                received,
            )
        } else if streaming {
            (
                native.response["usage"].clone(),
                completed && parse_ok && native.stopped,
                native.text_bytes,
            )
        } else {
            let value: Value = serde_json::from_slice(&json_body).unwrap_or(Value::Null);
            (
                value["usage"].clone(),
                completed && parse_ok && value["usage"].is_object(),
                received,
            )
        };
        let outcome = if !status.is_success() || responses.failed {
            "upstream_error"
        } else if completed && final_usage {
            "completed"
        } else {
            "interrupted"
        };
        if let Err(error) = finish_usage(
            &app,
            &claims,
            request_id,
            usage,
            output_bytes,
            body.len(),
            final_usage,
            outcome,
            upstream_status,
        )
        .await
        {
            tracing::error!(%request_id,%error,"usage finalization failed");
        }
        // Close the response after accounting. A disconnected client still leaves an estimated record.
        drop(sender);
    });
    Ok(response.body(Body::from_stream(
        tokio_stream::wrappers::ReceiverStream::new(receiver),
    ))?)
}
#[allow(clippy::too_many_arguments)]
async fn finish_usage(
    app: &App,
    claims: &Claims,
    id: Uuid,
    usage: Value,
    output_bytes: usize,
    input_bytes: usize,
    final_usage: bool,
    status: &str,
    upstream_status: Option<i32>,
) -> anyhow::Result<()> {
    let input = usage["input_tokens"]
        .as_i64()
        .unwrap_or(input_bytes.div_ceil(4) as i64);
    let output = usage["output_tokens"]
        .as_i64()
        .unwrap_or(output_bytes.div_ceil(4) as i64);
    let raw = if usage.is_object() { usage } else { json!({}) };
    let mut tx = app.store.tx(claims.org_id).await?;
    sqlx::query("UPDATE usage_records SET input_tokens=$3,output_tokens=$4,raw_usage=$5,estimated=$6,status=$7,upstream_status=$8,finished_at=now() WHERE org_id=$1 AND id=$2").bind(claims.org_id).bind(id).bind(input).bind(output).bind(raw).bind(!final_usage).bind(status).bind(upstream_status).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn issue_sandbox_key(app: &App, claims: &mut Claims) -> anyhow::Result<String> {
    ensure!(
        claims.kind == "sandbox" && claims.sandbox_id.is_some() && claims.lease_epoch.is_some(),
        "INVALID_INPUT"
    );
    let mut tx = app.store.tx(claims.org_id).await?;
    let ctx = context_in(&mut tx, claims.user_id, claims.org_id).await?;
    let _ = crate::store::conversation_in(&mut tx, &ctx, claims.conversation_id).await?;
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sandboxes WHERE org_id=$1 AND id=$2 AND run_id=$3 AND conversation_id=$4 AND lease_epoch=$5 AND state='running')").bind(claims.org_id).bind(claims.sandbox_id).bind(claims.run_id).bind(claims.conversation_id).bind(claims.lease_epoch).fetch_one(&mut *tx).await?;
    ensure!(valid, "FORBIDDEN");
    claims.exp = chrono::Utc::now().timestamp() + 3600;
    claims.jti = Uuid::now_v7();
    claims.exec_epoch = None;
    let token = identity::sign(&app.config.llm, claims)?;
    sqlx::query("UPDATE virtual_keys SET superseded_at=now() WHERE org_id=$1 AND sandbox_id=$2 AND superseded_at IS NULL").bind(claims.org_id).bind(claims.sandbox_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO virtual_keys(id,org_id,user_id,run_id,sandbox_id,lease_epoch,models,credential_id,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,to_timestamp($9::double precision))").bind(claims.jti).bind(claims.org_id).bind(claims.user_id).bind(claims.run_id).bind(claims.sandbox_id).bind(claims.lease_epoch).bind(&claims.models).bind(claims.credential_id).bind(claims.exp as f64).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(token)
}

pub async fn usage(
    State(app): State<App>,
    headers: HeaderMap,
    axum::extract::Path(org): axum::extract::Path<Uuid>,
) -> ApiResult<axum::Json<Vec<Value>>> {
    let auth = crate::api::authenticate(&app, &headers).await?;
    let ctx = app.store.context(auth.user_id, org).await?;
    let all = authorize(
        &Principal::User(ctx),
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
    let rows=sqlx::query_scalar("SELECT to_jsonb(u) FROM usage_records u WHERE org_id=$1 AND ($3 OR user_id=$2) ORDER BY created_at DESC LIMIT 100").bind(org).bind(auth.user_id).bind(all).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(axum::Json(rows))
}

pub fn error_response(code: &str) -> Response {
    let status = match code {
        "RATE_LIMITED" => StatusCode::TOO_MANY_REQUESTS,
        "FENCED" => StatusCode::CONFLICT,
        "MODEL_DENIED" | "PLATFORM_CREDENTIAL_DENIED" => StatusCode::FORBIDDEN,
        _ => StatusCode::BAD_GATEWAY,
    };
    (status, axum::Json(json!({"error":code}))).into_response()
}

async fn validate_stream(
    app: &App,
    claims: &Claims,
    model: &str,
    credential: Option<Uuid>,
) -> anyhow::Result<()> {
    let mut tx = app.store.tx(claims.org_id).await?;
    context_in(&mut tx, claims.user_id, claims.org_id).await?;
    if claims.kind == "loop" {
        validate(app, &mut tx, claims, model).await?;
    } else {
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sandboxes s JOIN runs r ON r.org_id=s.org_id AND r.id=$3 JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id JOIN virtual_keys v ON v.org_id=s.org_id AND v.id=$5 WHERE s.org_id=$1 AND s.id=$2 AND s.state='running' AND s.lease_epoch=$4 AND (s.node_lease_until IS NULL OR s.node_lease_until>clock_timestamp()) AND r.state IN ('running','requires_action') AND c.state='active' AND v.revoked_at IS NULL)").bind(claims.org_id).bind(claims.sandbox_id).bind(claims.run_id).bind(claims.lease_epoch).bind(claims.jti).fetch_one(&mut *tx).await?;
        ensure!(valid, "FENCED");
    }
    let policy = effective_policy(app, &mut tx, claims.org_id).await?;
    ensure!(
        policy.models.as_ref().is_none_or(|m| m.contains(model)),
        "MODEL_DENIED"
    );
    if let Some(id) = credential {
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM model_credentials WHERE org_id=$1 AND id=$2 AND revoked_at IS NULL AND (owner_user_id IS NULL OR (owner_user_id=$3 AND $4)))").bind(claims.org_id).bind(id).bind(claims.user_id).bind(policy.allow_personal_credentials!=Some(false)).fetch_one(&mut *tx).await?;
        ensure!(valid, "CREDENTIAL_REVOKED");
    } else {
        ensure!(
            policy.allow_platform_credentials != Some(false),
            "PLATFORM_CREDENTIAL_DENIED"
        );
    }
    tx.commit().await?;
    Ok(())
}
