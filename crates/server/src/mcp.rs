//! MCP gateway: what Work harnesses reach as the `ryzhub` MCP server. It offers the run's plugin
//! tools, and the built-in memory tools, which enforce live memory policy and private scope here.
//! A plugin tool the policy marks (`approval_required_tools`) runs only once the member approves
//! that call.
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
    plugins::{RunTool, client::Reply, elicit},
    runs,
    store::{ActionRow, Lease},
};

/// How often a blocked tool call re-reads its approval.
const APPROVAL_POLL: Duration = Duration::from_millis(500);

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
            let mut tools: Vec<Value> = vec![];
            if app.config().search.enabled {
                tools.extend(crate::memory::definitions().into_iter().map(|t|json!({"name":t["name"],"description":t["description"],"inputSchema":t["input_schema"]})));
            }
            let mut tx = app.store.tx(key.org_id).await?;
            let plugins = crate::plugins::run_tools(&app, &mut tx, key.org_id, key.run_id).await?;
            tx.commit().await?;
            tools.extend(plugins.iter().map(RunTool::mcp_definition));
            return Ok(json!({ "tools": tools }));
        }
        "tools/call" => {}
        _ => anyhow::bail!(Code::MethodUnsupported),
    }
    let name = request["params"]["name"]
        .as_str()
        .context(Code::InvalidTool)?;
    let session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("stateless");
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
    let (tool, required) = {
        let mut tx = app.store.tx(key.org_id).await?;
        let tools = crate::plugins::run_tools(&app, &mut tx, key.org_id, key.run_id).await?;
        let policy =
            crate::store::effective_policy(&mut tx, &app.config().policy, key.org_id).await?;
        tx.commit().await?;
        let tool = tools
            .into_iter()
            .find(|t| t.name == name)
            .context(Code::ToolDenied)?;
        let required = policy
            .approval_required_tools
            .as_ref()
            .is_some_and(|patterns| ryzhub_domain::tool_matches(patterns, &tool.name));
        (tool, required)
    };
    // Approved in the harness already: that approval covers this one call.
    if !required || consume_grant(&app, &key, name, &args).await? {
        return plugin_result(&app, &key, &tool, args).await;
    }
    ensure!(request.get("id").is_some(), Code::RequestIdRequired);
    let request_key = format!("{}:{}:{}", key.run_id, session, request["id"]);
    ensure!(request_key.len() < 1024, Code::RequestIdRequired);
    let id = register(&app, &key, &request_key, &tool, &args, Ask::Approval).await?;
    if let Some(result) = wait(&app, &key, id).await? {
        return Ok(result);
    }
    let outcome = plugin_result(&app, &key, &tool, args).await;
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

/// Call a run's plugin tool for the harness. A question its server asks is put to the member
/// as approvals are: the Owner shows it, the member answers in the client.
async fn plugin_result(
    app: &App,
    key: &GatewayKey,
    tool: &RunTool,
    args: Value,
) -> anyhow::Result<Value> {
    let mut answers = None;
    // A server may ask several questions in turn.
    for _ in 0..5 {
        match crate::plugins::call(app, key.org_id, tool, args.clone(), answers.take()).await? {
            Reply::Done(result) => return Ok(result),
            Reply::InputRequired { requests, state } => {
                let Some((question, form)) = elicit::question(&requests) else {
                    break;
                };
                let input = json!({"requests": requests, "state": state, "key": question});
                let request_key = format!("elicit:{}", Uuid::now_v7());
                let ask = Ask::Question { form, input };
                let id = register(app, key, &request_key, tool, &args, ask).await?;
                let answer = wait(app, key, id).await?;
                let answer = answer
                    .as_ref()
                    .and_then(Value::as_str)
                    .context(Code::ApprovalDeniedOrExpired)?;
                answers = Some((elicit::responses(&requests, &question, answer)?, state));
            }
        }
    }
    Ok(
        json!({"content":[{"type":"text","text":"The tool needs input the platform cannot give."}],"isError":true}),
    )
}

/// The digest that binds an approval to one call: its tool and arguments. serde_json's ordered
/// maps give a deterministic canonical form.
pub fn digest(tool: &str, args: &Value) -> String {
    let call = json!({"tool": tool, "arguments": args});
    format!("{:x}", Sha256::digest(call.to_string()))
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
    .bind(digest(tool, args))
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(used.is_some())
}

/// What the member is asked about a call.
enum Ask {
    /// Whether it may run.
    Approval,
    /// A plugin server's question: its form, and what the answer goes back to the server with.
    Question { form: Value, input: Value },
}

/// Record the call, once per request key, with the member's pending action. Returns its id.
async fn register(
    app: &App,
    key: &GatewayKey,
    request_key: &str,
    tool: &RunTool,
    args: &Value,
    ask: Ask,
) -> anyhow::Result<Uuid> {
    let sandbox = key.sandbox_id.context(Code::SandboxIdentityRequired)?;
    let epoch = key.lease_epoch.context(Code::SandboxIdentityRequired)?;
    let digest = digest(&tool.name, args);
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
    if let Some((id, old)) = old {
        ensure!(old == digest, Code::RequestIdReused);
        tx.commit().await?;
        return Ok(id);
    }
    let (kind, summary, options, form, input): (_, String, &[&str], _, _) = match ask {
        Ask::Approval => (
            "approval",
            format!("MCP 工具 {}\n{args}", tool.name),
            &["allow", "deny"],
            None,
            None,
        ),
        Ask::Question { form, input } => {
            let message = form["message"].as_str().unwrap_or("需要你提供信息");
            let summary = format!("{}：{message}", tool.plugin);
            let summary = summary.chars().take(4000).collect();
            ("question", summary, &[], Some(form), Some(input))
        }
    };
    let id = Uuid::now_v7();
    let action = Uuid::now_v7();
    let ttl = app.config().approval_ttl_secs as i64;
    sqlx::query(
        "INSERT INTO pending_actions(id,org_id,run_id,run_epoch,kind,summary,options,expires_at,form)
         SELECT $1,$2,id,exec_epoch,$4,$5,$6,now()+($7::bigint*interval '1 second'),$8
         FROM runs WHERE org_id=$2 AND id=$3",
    )
    .bind(action)
    .bind(key.org_id)
    .bind(key.run_id)
    .bind(kind)
    .bind(summary)
    .bind(options)
    .bind(ttl)
    .bind(form)
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
    .bind(request_key)
    .bind(&digest)
    .bind(&tool.name)
    .bind(args)
    .bind(action)
    .bind(ttl)
    .bind(tool.installation)
    .bind(input)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Block until the member decides about call `id`. None: approved, and now `executing`, to run
/// once. Otherwise what it completed with: an earlier result of the same request, or the answer
/// to a question.
async fn wait(app: &App, key: &GatewayKey, id: Uuid) -> anyhow::Result<Option<Value>> {
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
            "SELECT c.state,coalesce(c.result,c.input->'answer'),a.status,c.expires_at>now(),c.lease_epoch
             FROM mcp_calls c JOIN pending_actions a ON a.org_id=c.org_id AND a.id=c.action_id
             WHERE c.org_id=$1 AND c.id=$2 FOR UPDATE OF c",
        )
        .bind(key.org_id)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(key.lease_epoch == Some(lease_epoch), Code::Fenced);
        if state == "completed" {
            return Ok(Some(result.context("completed call without result")?));
        }
        ensure!(
            !matches!(state.as_str(), "executing" | "outcome_unknown"),
            Code::OutcomeUnknownDoNotRetry
        );
        ensure!(
            unexpired && matches!(decision.as_str(), "pending" | "allow"),
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
