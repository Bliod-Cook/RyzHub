//! MCP gateway. Remote tools require one-use user approval outside the sandbox.
//! Built-in memory tools instead enforce live memory policy and private scope here.
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
            let mut tools: Vec<Value> = app
                .config
                .work
                .mcp_tools
                .iter()
                .map(|t| json!({"name":t.name,"description":t.description,"inputSchema":t.input_schema}))
                .collect();
            if app.config.search.enabled {
                tools.extend(crate::memory::definitions().into_iter().map(|t|json!({"name":t["name"],"description":t["description"],"inputSchema":t["input_schema"]})));
            }
            let mut tx = app.store.tx(key.org_id).await?;
            let plugins = crate::plugins::run_tools(&app, &mut tx, key.org_id, key.run_id).await?;
            tx.commit().await?;
            tools.extend(plugins.iter().map(crate::plugins::RunTool::mcp_definition));
            return Ok(json!({ "tools": tools }));
        }
        "tools/call" => {}
        _ => anyhow::bail!(Code::MethodUnsupported),
    }
    let name = request["params"]["name"]
        .as_str()
        .context(Code::InvalidTool)?;
    if crate::memory::TOOLS.contains(&name) {
        ensure!(request.get("id").is_some(), Code::RequestIdRequired);
        let args = request["params"]
            .get("arguments")
            .cloned()
            .unwrap_or(json!({}));
        let vector =
            crate::memory::prepare_search(&app, key.org_id, key.run_id, name, &args).await?;
        let mut tx = app.store.tx(key.org_id).await?;
        validate_key(&mut tx, &key, false).await?;
        let session = headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("stateless");
        let result = crate::memory::execute(
            &mut tx,
            &app,
            key.org_id,
            key.run_id,
            name,
            args,
            format!(
                "mcp:{}:{:?}:{:?}:{}:{}",
                key.run_id, key.sandbox_id, key.lease_epoch, session, request["id"]
            ),
            vector,
        )
        .await?;
        tx.commit().await?;
        return Ok(json!({"content":[{"type":"text","text":result.to_string()}],"isError":false}));
    }
    let args = request["params"]
        .get("arguments")
        .cloned()
        .unwrap_or(json!({}));
    ensure!(args.is_object(), Code::InvalidArguments);
    // A plugin tool: allowed, denied, or (policy) approved once by the member.
    let plugin_tool = {
        let mut tx = app.store.tx(key.org_id).await?;
        let tools = crate::plugins::run_tools(&app, &mut tx, key.org_id, key.run_id).await?;
        let policy =
            crate::store::effective_policy(&mut tx, &app.config.policy, key.org_id).await?;
        tx.commit().await?;
        tools.into_iter().find(|t| t.name == name).map(|tool| {
            let required = policy
                .approval_required_tools
                .as_ref()
                .is_some_and(|patterns| ryzhub_domain::tool_matches(patterns, &tool.name));
            (tool, required)
        })
    };
    if let Some((tool, false)) = &plugin_tool {
        return plugin_result(&app, &key, tool, args).await;
    }
    let configured = app.config.work.mcp_tools.iter().find(|t| t.name == name);
    ensure!(
        configured.is_some() || plugin_tool.is_some(),
        Code::ToolDenied
    );
    // serde_json's ordered maps give a deterministic canonical form.
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&json!({"tool":name,"arguments":args}))?)
    );
    // Approved in the harness already: that approval covers this one call.
    if let Some((tool, true)) = &plugin_tool
        && consume_grant(&app, &key, name, &args).await?
    {
        return plugin_result(&app, &key, tool, args).await;
    }
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
    let outcome = match (&plugin_tool, configured) {
        (Some((tool, _)), _) => plugin_result(&app, &key, tool, args).await,
        (None, Some(tool)) => remote_call(&app, tool, id, args).await,
        (None, None) => Err(Code::ToolDenied.into()),
    };
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

/// Call a run's plugin tool for the harness.
async fn plugin_result(
    app: &App,
    key: &GatewayKey,
    tool: &crate::plugins::RunTool,
    args: Value,
) -> anyhow::Result<Value> {
    let mut answers = None;
    // A server may ask several questions in turn.
    for _ in 0..5 {
        match crate::plugins::call(app, key.org_id, tool, args.clone(), answers.take()).await? {
            crate::plugins::client::Reply::Done(result) => return Ok(result),
            crate::plugins::client::Reply::InputRequired { requests, state } => {
                let Some((question, form)) = crate::plugins::elicit::question(&requests) else {
                    break;
                };
                let answer =
                    ask_member(app, key, tool, &args, &requests, &state, &question, form).await?;
                let responses = crate::plugins::elicit::responses(&requests, &question, &answer)
                    .unwrap_or_else(|_| json!({}));
                answers = Some((responses, state));
            }
        }
    }
    Ok(
        json!({"content":[{"type":"text","text":"The tool needs input the platform cannot give."}],"isError":true}),
    )
}

/// Ask the member a plugin server's question and wait for the answer, as approvals wait: the
/// Owner shows it, the member answers in the client.
#[allow(clippy::too_many_arguments)]
async fn ask_member(
    app: &App,
    key: &GatewayKey,
    tool: &crate::plugins::RunTool,
    args: &Value,
    requests: &Value,
    state: &Option<String>,
    question: &str,
    form: Value,
) -> anyhow::Result<String> {
    let sandbox = key.sandbox_id.context(Code::SandboxIdentityRequired)?;
    let epoch = key.lease_epoch.context(Code::SandboxIdentityRequired)?;
    let id = Uuid::now_v7();
    let action = Uuid::now_v7();
    let ttl = app.config.approval_ttl_secs as i64;
    let summary: String = format!(
        "{}：{}",
        tool.plugin,
        form["message"].as_str().unwrap_or("需要你提供信息")
    )
    .chars()
    .take(4000)
    .collect();
    let mut tx = app.store.tx(key.org_id).await?;
    sqlx::query(
        "INSERT INTO pending_actions(id,org_id,run_id,run_epoch,kind,summary,options,expires_at,form)
         SELECT $1,$2,id,exec_epoch,'question',$4,ARRAY['__answer__'],now()+($5::bigint*interval '1 second'),$6
         FROM runs WHERE org_id=$2 AND id=$3",
    )
    .bind(action)
    .bind(key.org_id)
    .bind(key.run_id)
    .bind(&summary)
    .bind(ttl)
    .bind(&form)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO mcp_calls(id,org_id,run_id,sandbox_id,lease_epoch,request_key,fingerprint,tool,arguments,
             action_id,expires_at,installation_id,input)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,now()+($11::bigint*interval '1 second'),$12,$13)",
    )
    .bind(id)
    .bind(key.org_id)
    .bind(key.run_id)
    .bind(sandbox)
    .bind(epoch)
    .bind(format!("elicit:{id}"))
    .bind(args_digest(args))
    .bind(&tool.name)
    .bind(args)
    .bind(action)
    .bind(ttl)
    .bind(tool.installation)
    .bind(json!({"requests": requests, "state": state, "key": question}))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    loop {
        let mut tx = app.store.tx(key.org_id).await?;
        validate_key(&mut tx, key, false).await?;
        let (answer, status, unexpired): (Option<String>, String, bool) = sqlx::query_as(
            "SELECT c.input->>'answer',a.status,c.expires_at>now() FROM mcp_calls c
             JOIN pending_actions a ON a.org_id=c.org_id AND a.id=c.action_id
             WHERE c.org_id=$1 AND c.id=$2",
        )
        .bind(key.org_id)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        if let Some(answer) = answer {
            return Ok(answer);
        }
        ensure!(
            unexpired && status == "pending",
            Code::ApprovalDeniedOrExpired
        );
        tokio::time::sleep(APPROVAL_POLL).await;
    }
}

/// The digest one-use approvals bind a call's arguments by.
pub fn args_digest(args: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(args).unwrap_or_default())
    )
}

/// Use up an unexpired approval of this exact call made in the harness during this execution.
async fn consume_grant(
    app: &App,
    key: &GatewayKey,
    tool: &str,
    args: &Value,
) -> anyhow::Result<bool> {
    let mut tx = app.store.tx(key.org_id).await?;
    let used: Option<Uuid> = sqlx::query_scalar(
        "UPDATE approval_grants g SET consumed_at=now() FROM runs r
         WHERE g.id=(SELECT id FROM approval_grants WHERE org_id=$1 AND run_id=$2 AND tool=$3 AND args_digest=$4
                       AND consumed_at IS NULL AND expires_at>now() ORDER BY created_at LIMIT 1 FOR UPDATE SKIP LOCKED)
           AND r.org_id=g.org_id AND r.id=g.run_id AND r.exec_epoch=g.exec_epoch
         RETURNING g.id",
    )
    .bind(key.org_id)
    .bind(key.run_id)
    .bind(tool)
    .bind(args_digest(args))
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(used.is_some())
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
    crate::memory::project(&mut tx, lease).await?;
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
