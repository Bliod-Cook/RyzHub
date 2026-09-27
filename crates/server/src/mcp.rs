//! MCP gateway. Every tool call needs a one-use user approval enforced here, outside the
//! sandbox, so a harness (or code inside the sandbox) cannot bypass it.
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;
use uuid::Uuid;

use crate::{
    app::App,
    error::{Code, public_code},
    llm::{
        gateway::{bearer, validate_key},
        keys::{self, GatewayKey, KeyKind},
    },
    runs,
    store::{ActionRow, Lease},
};

/// How often a blocked tool call re-reads its approval.
const APPROVAL_POLL: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub url: String,
    pub remote_name: String,
}

pub async fn endpoint(
    State(app): State<App>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    let id = request["id"].clone();
    match call(app, headers, request).await {
        Ok(_) if id.is_null() => StatusCode::ACCEPTED.into_response(),
        Ok(value) => Json(json!({"jsonrpc":"2.0","id":id,"result":value})).into_response(),
        Err(error) => {
            let message = public_code(&error).to_string();
            Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":message}}))
                .into_response()
        }
    }
}

/// The caller must present a live sandbox key. MCP is not a model call: no model check.
async fn authorize(app: &App, headers: &HeaderMap) -> anyhow::Result<GatewayKey> {
    let key = keys::resolve(&app.store, bearer(headers)?).await?;
    ensure!(
        key.kind() == KeyKind::Sandbox,
        Code::SandboxIdentityRequired
    );
    let mut tx = app.store.tx(key.org_id).await?;
    validate_key(&mut tx, &key, false).await?;
    tx.commit().await?;
    Ok(key)
}

async fn call(app: App, headers: HeaderMap, request: Value) -> anyhow::Result<Value> {
    let key = authorize(&app, &headers).await?;
    let method = request["method"]
        .as_str()
        .context(Code::MethodUnsupported)?;
    match method {
        "initialize" => {
            return Ok(
                json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{"listChanged":false}},
                             "serverInfo":{"name":"ryzhub-mcp-gateway","version":"0.2.0"}}),
            );
        }
        "notifications/initialized" | "ping" => return Ok(json!({})),
        "tools/list" => {
            let tools: Vec<Value> = app
                .config
                .work
                .mcp_tools
                .iter()
                .map(|t| json!({"name":t.name,"description":t.description,"inputSchema":t.input_schema}))
                .collect();
            return Ok(json!({ "tools": tools }));
        }
        "tools/call" => {}
        _ => anyhow::bail!(Code::MethodUnsupported),
    }
    let name = request["params"]["name"]
        .as_str()
        .context(Code::InvalidTool)?;
    let tool = app
        .config
        .work
        .mcp_tools
        .iter()
        .find(|t| t.name == name)
        .context(Code::ToolDenied)?;
    let args = request["params"]
        .get("arguments")
        .cloned()
        .unwrap_or(json!({}));
    ensure!(args.is_object(), Code::InvalidArguments);
    // serde_json's ordered maps give a deterministic canonical form.
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&json!({"tool":name,"arguments":args}))?)
    );
    let session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("stateless");
    ensure!(request.get("id").is_some(), Code::RequestIdRequired);
    let request_key = format!("{}:{}:{}", key.run_id, session, request["id"]);
    ensure!(request_key.len() < 1024, Code::RequestIdRequired);
    let sandbox = key.sandbox_id.context(Code::SandboxIdentityRequired)?;
    let epoch = key.lease_epoch.context(Code::SandboxIdentityRequired)?;
    let id = register(
        &app,
        &key,
        sandbox,
        epoch,
        &request_key,
        &fingerprint,
        name,
        &args,
    )
    .await?;
    if let Some(result) = wait_for_approval(&app, &key, id, epoch).await? {
        return Ok(result);
    }
    let outcome = remote_call(&app, tool, id, args).await;
    let mut tx = app.store.tx(key.org_id).await?;
    sqlx::query("UPDATE mcp_calls SET state=$3,result=$4 WHERE org_id=$1 AND id=$2")
        .bind(key.org_id)
        .bind(id)
        .bind(if outcome.is_ok() {
            "completed"
        } else {
            "outcome_unknown"
        })
        .bind(outcome.as_ref().ok())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    outcome
}

/// Record the call (idempotently per request id) with a pending approval.
#[allow(clippy::too_many_arguments)]
async fn register(
    app: &App,
    key: &GatewayKey,
    sandbox: Uuid,
    epoch: i64,
    request_key: &str,
    fingerprint: &str,
    name: &str,
    args: &Value,
) -> anyhow::Result<Uuid> {
    let mut tx = app.store.tx(key.org_id).await?;
    // Serializes duplicate requests without holding a lock across the upstream call.
    sqlx::query("SELECT id FROM sandboxes WHERE org_id=$1 AND id=$2 FOR UPDATE")
        .bind(key.org_id)
        .bind(sandbox)
        .fetch_one(&mut *tx)
        .await?;
    let old: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id,fingerprint FROM mcp_calls WHERE org_id=$1 AND sandbox_id=$2 AND request_key=$3",
    )
    .bind(key.org_id)
    .bind(sandbox)
    .bind(request_key)
    .fetch_optional(&mut *tx)
    .await?;
    let id = match old {
        Some((id, old)) => {
            ensure!(old == fingerprint, Code::RequestIdReused);
            id
        }
        None => {
            let id = Uuid::now_v7();
            let action = Uuid::now_v7();
            let ttl = app.config.approval_ttl_secs as i64;
            sqlx::query(
                "INSERT INTO pending_actions(id,org_id,run_id,run_epoch,kind,summary,options,expires_at)
                 SELECT $1,$2,id,exec_epoch,'approval',$4,ARRAY['allow','deny'],now()+($5::bigint*interval '1 second')
                 FROM runs WHERE org_id=$2 AND id=$3",
            )
            .bind(action)
            .bind(key.org_id)
            .bind(key.run_id)
            .bind(format!("MCP 工具 {name}\n{args}"))
            .bind(ttl)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO mcp_calls(id,org_id,run_id,sandbox_id,lease_epoch,request_key,fingerprint,tool,arguments,action_id,expires_at)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,now()+($11::bigint*interval '1 second'))",
            )
            .bind(id)
            .bind(key.org_id)
            .bind(key.run_id)
            .bind(sandbox)
            .bind(epoch)
            .bind(request_key)
            .bind(fingerprint)
            .bind(name)
            .bind(args)
            .bind(action)
            .bind(ttl)
            .execute(&mut *tx)
            .await?;
            id
        }
    };
    tx.commit().await?;
    Ok(id)
}

/// Block until the call is approved (returns None, and marks it executing), was already
/// completed (returns its result), or can no longer run.
async fn wait_for_approval(
    app: &App,
    key: &GatewayKey,
    id: Uuid,
    epoch: i64,
) -> anyhow::Result<Option<Value>> {
    loop {
        let mut tx = app.store.tx(key.org_id).await?;
        validate_key(&mut tx, key, false).await?;
        let (state, result, decision, unexpired, lease_epoch): (
            String,
            Option<Value>,
            String,
            bool,
            i64,
        ) = sqlx::query_as(
            "SELECT c.state,c.result,a.status,c.expires_at>now(),c.lease_epoch FROM mcp_calls c
                 JOIN pending_actions a ON a.org_id=c.org_id AND a.id=c.action_id
                 WHERE c.org_id=$1 AND c.id=$2 FOR UPDATE OF c",
        )
        .bind(key.org_id)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(lease_epoch == epoch, Code::Fenced);
        if state == "completed" {
            return Ok(Some(result.context("completed call without result")?));
        }
        ensure!(
            !matches!(state.as_str(), "executing" | "outcome_unknown"),
            Code::OutcomeUnknownDoNotRetry
        );
        ensure!(
            unexpired && decision != "deny" && decision != "invalidated",
            Code::ApprovalDeniedOrExpired
        );
        if decision == "allow" {
            sqlx::query("UPDATE mcp_calls SET state='executing' WHERE org_id=$1 AND id=$2")
                .bind(key.org_id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(None);
        }
        tx.commit().await?;
        tokio::time::sleep(APPROVAL_POLL).await;
    }
}

/// Only the Owner turns gateway requests into ordered timeline events.
pub async fn project(app: &App, lease: &Lease, run: Uuid) -> anyhow::Result<()> {
    let mut tx = app.store.fenced(lease).await?;
    let pending: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id,action_id FROM mcp_calls WHERE org_id=$1 AND run_id=$2 AND state='pending' FOR UPDATE",
    )
    .bind(lease.org_id)
    .bind(run)
    .fetch_all(&mut *tx)
    .await?;
    for (id, action) in pending {
        let action: ActionRow = sqlx::query_as(concat!(
            "SELECT ",
            action_columns!(),
            " FROM pending_actions WHERE org_id=$1 AND id=$2"
        ))
        .bind(lease.org_id)
        .bind(action)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("UPDATE mcp_calls SET state='awaiting_approval' WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        runs::request_action(&mut tx, lease, run, action).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn remote_call(app: &App, tool: &Tool, id: Uuid, args: Value) -> anyhow::Result<Value> {
    let client = &app.http.tools;
    let init = client
        .post(&tool.url)
        .header("accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":format!("{id}:initialize"),"method":"initialize",
                      "params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"ryzhub","version":"0.2.0"}}}))
        .send()
        .await?;
    ensure!(init.status().is_success(), Code::McpUpstreamFailed);
    let session = init.headers().get("mcp-session-id").cloned();
    let init = remote_response(init).await?;
    ensure!(init.get("error").is_none(), Code::McpUpstreamFailed);
    let version = init["result"]["protocolVersion"]
        .as_str()
        .unwrap_or("2025-03-26")
        .to_owned();
    let request = |body: Value| {
        let mut request = client
            .post(&tool.url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", version.as_str())
            .json(&body);
        if let Some(session) = &session {
            request = request.header("mcp-session-id", session);
        }
        request
    };
    request(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await?
        .error_for_status()?;
    let response = request(json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                                  "params":{"name":tool.remote_name,"arguments":args}}))
    .send()
    .await?;
    let response = remote_response(response).await?;
    ensure!(response.get("error").is_none(), Code::McpUpstreamFailed);
    Ok(response["result"].clone())
}

async fn remote_response(response: reqwest::Response) -> anyhow::Result<Value> {
    use futures_util::StreamExt;
    ensure!(response.status().is_success(), Code::McpUpstreamFailed);
    let streaming = response
        .headers()
        .get("content-type")
        .is_some_and(|v| v.to_str().unwrap_or("").starts_with("text/event-stream"));
    if streaming {
        let mut decoder = crate::provider::SseDecoder::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            for event in decoder.push(&chunk?)? {
                if event.get("result").is_some() || event.get("error").is_some() {
                    return Ok(event);
                }
            }
        }
        anyhow::bail!(Code::McpUpstreamFailed)
    }
    let bytes = response.bytes().await?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, Code::McpUpstreamFailed);
    Ok(serde_json::from_slice(&bytes)?)
}
