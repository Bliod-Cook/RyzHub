//! Gateway-enforced, single-use tool grants. Native harness approvals cannot bypass this gate.
use crate::{
    api::App,
    chat,
    llm::{gateway, identity},
    store::{ActionRow, Lease, append},
};
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use ryzhub_proto::{RunState, event_envelope::Event};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub url: String,
    pub remote_name: String,
}
fn result(id: &Value, value: Value) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"result":value})).into_response()
}
pub async fn endpoint(
    State(app): State<App>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    let id = request["id"].clone();
    match call(app, headers, request).await {
        Ok(_) if id.is_null() => StatusCode::ACCEPTED.into_response(),
        Ok(v) => result(&id, v),
        Err(e) => (
            StatusCode::OK,
            Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":e.to_string()}})),
        )
            .into_response(),
    }
}
async fn call(app: App, headers: HeaderMap, request: Value) -> anyhow::Result<Value> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .context("UNAUTHORIZED")?;
    let claims = identity::verify(&app.config.llm, token)?;
    ensure!(claims.kind == "sandbox", "SANDBOX_IDENTITY_REQUIRED");
    let mut tx = app.store.tx(claims.org_id).await?;
    gateway::validate(&app, &mut tx, &claims, &app.config.work.model).await?;
    tx.commit().await?;
    let method = request["method"].as_str().context("INVALID_METHOD")?;
    match method {
        "initialize" => {
            return Ok(
                json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"ryzhub-mcp-gateway","version":"0.2.0"}}),
            );
        }
        "notifications/initialized" | "ping" => return Ok(json!({})),
        "tools/list" => {
            return Ok(
                json!({"tools":app.config.work.mcp_tools.iter().map(|t|json!({"name":t.name,"description":t.description,"inputSchema":t.input_schema})).collect::<Vec<_>>()}),
            );
        }
        "tools/call" => {}
        _ => anyhow::bail!("METHOD_UNSUPPORTED"),
    }
    let name = request["params"]["name"].as_str().context("INVALID_TOOL")?;
    let tool = app
        .config
        .work
        .mcp_tools
        .iter()
        .find(|t| t.name == name)
        .context("TOOL_DENIED")?;
    let args = request["params"]
        .get("arguments")
        .cloned()
        .unwrap_or(json!({}));
    ensure!(args.is_object(), "INVALID_ARGUMENTS");
    // serde_json's ordered object map yields a deterministic canonical representation.
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&json!({"tool":name,"arguments":args}))?)
    );
    let session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("stateless");
    let request_key = format!("{}:{}:{}", claims.run_id, session, request["id"]);
    ensure!(
        request_key.len() < 1024 && request.get("id").is_some(),
        "REQUEST_ID_REQUIRED"
    );
    let sandbox = claims.sandbox_id.context("SANDBOX_IDENTITY_REQUIRED")?;
    let epoch = claims.lease_epoch.context("SANDBOX_IDENTITY_REQUIRED")?;
    let mut tx = app.store.tx(claims.org_id).await?;
    // Serializes duplicate requests without holding any locks across the upstream call.
    sqlx::query("SELECT id FROM sandboxes WHERE org_id=$1 AND id=$2 FOR UPDATE")
        .bind(claims.org_id)
        .bind(sandbox)
        .fetch_one(&mut *tx)
        .await?;
    let old: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id,fingerprint FROM mcp_calls WHERE org_id=$1 AND sandbox_id=$2 AND request_key=$3",
    )
    .bind(claims.org_id)
    .bind(sandbox)
    .bind(&request_key)
    .fetch_optional(&mut *tx)
    .await?;
    let id = if let Some((id, old)) = old {
        ensure!(old == fingerprint, "REQUEST_ID_REUSED");
        id
    } else {
        let id = Uuid::now_v7();
        let action = Uuid::now_v7();
        sqlx::query("INSERT INTO pending_actions(id,org_id,run_id,run_epoch,kind,summary,options,expires_at) SELECT $1,$2,id,exec_epoch,'approval',$4,ARRAY['allow','deny'],now()+interval '10 minutes' FROM runs WHERE org_id=$2 AND id=$3").bind(action).bind(claims.org_id).bind(claims.run_id).bind(format!("MCP 工具 {name}\n{args}")).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO mcp_calls(id,org_id,run_id,sandbox_id,lease_epoch,request_key,fingerprint,tool,arguments,action_id,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,now()+interval '10 minutes')").bind(id).bind(claims.org_id).bind(claims.run_id).bind(sandbox).bind(epoch).bind(request_key).bind(fingerprint).bind(name).bind(&args).bind(action).execute(&mut *tx).await?;
        id
    };
    tx.commit().await?;
    loop {
        let mut tx = app.store.tx(claims.org_id).await?;
        gateway::validate(&app, &mut tx, &claims, &app.config.work.model).await?;
        let row: (String,Option<Value>,String,bool,i64)=sqlx::query_as("SELECT c.state,c.result,a.status,c.expires_at>now(),c.lease_epoch FROM mcp_calls c JOIN pending_actions a ON a.org_id=c.org_id AND a.id=c.action_id WHERE c.org_id=$1 AND c.id=$2 FOR UPDATE OF c").bind(claims.org_id).bind(id).fetch_one(&mut *tx).await?;
        ensure!(row.4 == epoch, "FENCED");
        if row.0 == "completed" {
            return row.1.context("MISSING_RESULT");
        }
        ensure!(
            !matches!(row.0.as_str(), "executing" | "outcome_unknown"),
            "OUTCOME_UNKNOWN_DO_NOT_RETRY"
        );
        ensure!(
            row.3 && row.2 != "deny" && row.2 != "invalidated",
            "APPROVAL_DENIED_OR_EXPIRED"
        );
        if row.2 == "allow" {
            sqlx::query("UPDATE mcp_calls SET state='executing' WHERE org_id=$1 AND id=$2")
                .bind(claims.org_id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            break;
        }
        tx.commit().await?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let outcome = remote_call(tool, id, args).await;
    let mut tx = app.store.tx(claims.org_id).await?;
    sqlx::query("UPDATE mcp_calls SET state=$3,result=$4 WHERE org_id=$1 AND id=$2")
        .bind(claims.org_id)
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
/// Only the Owner turns gateway requests into ordered timeline events.
pub async fn project(app: &App, lease: &Lease, run: Uuid) -> anyhow::Result<()> {
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    let rows:Vec<(Uuid,Uuid)>=sqlx::query_as("SELECT id,action_id FROM mcp_calls WHERE org_id=$1 AND run_id=$2 AND state='pending' FOR UPDATE").bind(lease.org_id).bind(run).fetch_all(&mut *tx).await?;
    for (id, action) in rows {
        let row:ActionRow=sqlx::query_as("SELECT id,run_id,run_epoch,summary,options,status,resolved_device,expires_at FROM pending_actions WHERE org_id=$1 AND id=$2").bind(lease.org_id).bind(action).fetch_one(&mut *tx).await?;
        append(
            &mut tx,
            lease.org_id,
            &lease.channel(),
            Some(lease),
            "",
            Event::PendingActionChanged(row.into()),
        )
        .await?;
        sqlx::query("UPDATE mcp_calls SET state='awaiting_approval' WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE runs SET state='requires_action' WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(run)
            .execute(&mut *tx)
            .await?;
        append(
            &mut tx,
            lease.org_id,
            &lease.channel(),
            Some(lease),
            "",
            Event::RunStateChanged(RunState {
                run_id: run.to_string(),
                state: "requires_action".into(),
                exec_epoch: lease.epoch as u64,
                read_only: false,
            }),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn remote_call(tool: &Tool, id: Uuid, args: Value) -> anyhow::Result<Value> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let init=client.post(&tool.url).header("accept","application/json, text/event-stream").json(&json!({"jsonrpc":"2.0","id":format!("{id}:initialize"),"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"ryzhub","version":"0.2.0"}}})).send().await?;
    ensure!(init.status().is_success(), "MCP_INITIALIZE_FAILED");
    let session = init.headers().get("mcp-session-id").cloned();
    let init = remote_response(init).await?;
    ensure!(init.get("error").is_none(), "MCP_INITIALIZE_FAILED");
    let version = init["result"]["protocolVersion"]
        .as_str()
        .unwrap_or("2025-03-26");
    let request = |body: Value| {
        let mut r = client
            .post(&tool.url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", version)
            .json(&body);
        if let Some(session) = &session {
            r = r.header("mcp-session-id", session);
        }
        r
    };
    request(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await?
        .error_for_status()?;
    let response=request(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool.remote_name,"arguments":args}})).send().await?;
    let response = remote_response(response).await?;
    ensure!(response.get("error").is_none(), "MCP_UPSTREAM_ERROR");
    Ok(response["result"].clone())
}
async fn remote_response(response: reqwest::Response) -> anyhow::Result<Value> {
    use futures_util::StreamExt;
    ensure!(response.status().is_success(), "MCP_UPSTREAM_FAILED");
    if response
        .headers()
        .get("content-type")
        .is_some_and(|v| v.to_str().unwrap_or("").starts_with("text/event-stream"))
    {
        let mut decoder = crate::provider::SseDecoder::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            for event in decoder.push(&chunk?)? {
                if event.get("result").is_some() || event.get("error").is_some() {
                    return Ok(event);
                }
            }
        }
        anyhow::bail!("MCP_STREAM_INTERRUPTED")
    }
    let bytes = response.bytes().await?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, "MCP_RESPONSE_TOO_LARGE");
    Ok(serde_json::from_slice(&bytes)?)
}
