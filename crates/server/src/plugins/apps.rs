//! Plugin UI. An MCP App page (`ui://`) is read through the plugin's own server and served
//! from the plugin content origin, which is not the client's, by an unguessable short-lived
//! token. Its CSP lets it reach only the domains it declares, and only the client may frame
//! it; the client also sandboxes the frame. The page sees no conversation or token: the client
//! hands it the tool's input and result, and its tool calls come back here, checked against
//! the run's plugins like the model's.
use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::App,
    error::{ApiResult, Code},
    http::member_tx,
};

/// How long a page's address works.
const PAGE_MINUTES: i64 = 10;
const MAX_PAGE: usize = 2 * 1024 * 1024;

#[derive(Deserialize)]
pub struct AppRequest {
    item_id: Uuid,
}

/// The tool call shown by `item`, in a conversation of the caller: its run, tool, input and
/// result.
async fn tool_call(
    tx: &mut crate::store::Tx<'_>,
    org: Uuid,
    user: Uuid,
    conv: Uuid,
    item: Uuid,
) -> anyhow::Result<(Uuid, String, Value, Option<Value>)> {
    sqlx::query_as(
        "SELECT t.run_id,t.name,t.arguments,t.result FROM tool_calls t
         JOIN runs r ON r.org_id=t.org_id AND r.id=t.run_id
         JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id
         WHERE t.org_id=$1 AND t.item_id=$2 AND c.id=$3 AND c.creator_id=$4",
    )
    .bind(org)
    .bind(item)
    .bind(conv)
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?
    .context(Code::PluginNotFound)
}

/// `POST /orgs/{org}/conversations/{conv}/apps`: the address of the MCP App page that shows
/// a tool item, with the input and result to hand it, and what the run calls the tools of its
/// server, which the page knows by their names there.
pub async fn open(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Json(request): Json<AppRequest>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let (run, name, arguments, result) =
        tool_call(&mut tx, org, ctx.user_id, conv, request.item_id).await?;
    let tools = super::run_tools(&app, &mut tx, org, run).await?;
    tx.commit().await?;
    let tool = tools
        .iter()
        .find(|tool| tool.name == name)
        .context(Code::PluginNotFound)?;
    let names: serde_json::Map<String, Value> = tools
        .iter()
        .filter(|t| t.installation == tool.installation && t.server == tool.server)
        .map(|t| (t.remote.clone(), json!(t.name)))
        .collect();
    let uri = tool.ui.clone().context(Code::PluginNotFound)?;
    let session = super::session(&app, org, tool.installation, &tool.server).await?;
    let read = session.resource(&uri).await?;
    let content = read["contents"]
        .as_array()
        .and_then(|contents| {
            contents
                .iter()
                .find(|c| c["uri"] == uri.as_str())
                .or(contents.first())
        })
        .context(Code::McpUpstreamFailed)?;
    let html = match (content["text"].as_str(), content["blob"].as_str()) {
        (Some(text), _) => text.to_owned(),
        (None, Some(blob)) => String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(blob)
                .context(Code::McpUpstreamFailed)?,
        )
        .context(Code::McpUpstreamFailed)?,
        _ => return Err(Code::McpUpstreamFailed.into()),
    };
    require!(html.len() <= MAX_PAGE, Code::PluginTooLarge);
    let csp = policy(&app, &content["_meta"]["ui"]["csp"]);
    let token = {
        let random: [u8; 32] = rand::random();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
    };
    sqlx::query(
        "INSERT INTO plugin_app_pages(token,html,expires_at) VALUES($1,$2,now()+($3::bigint*interval '1 minute'))",
    )
    .bind(&token)
    .bind(json!({"html": html, "csp": csp}).to_string())
    .bind(PAGE_MINUTES)
    .execute(&app.store.pool)
    .await?;
    sqlx::query("DELETE FROM plugin_app_pages WHERE expires_at<now()")
        .execute(&app.store.pool)
        .await?;
    Ok(Json(json!({
        "url": format!("{}/apps/{token}", app.config().plugins.content_origin.trim_end_matches('/')),
        "origin": app.config().plugins.content_origin.trim_end_matches('/'),
        "tool": tool.name,
        "plugin": tool.plugin,
        "tools": names,
        "arguments": arguments,
        "result": result,
    })))
}

/// The page's CSP: scripts and styles inline or from HTTPS, connections only to the domains
/// the page declared, framed only by the client.
fn policy(app: &App, declared: &Value) -> String {
    let domains = |key: &str| -> String {
        declared[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|d| {
                d.starts_with("https://")
                    && d.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b".-:/*".contains(&b))
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let connect = domains("connectDomains");
    let resources = domains("resourceDomains");
    let ancestors = app.config().allowed_origins.join(" ");
    format!(
        "default-src 'none'; script-src 'unsafe-inline' {resources}; style-src 'unsafe-inline' {resources}; \
         img-src data: blob: {resources}; font-src data: {resources}; media-src data: blob: {resources}; \
         connect-src {}; frame-ancestors {ancestors}; base-uri 'none'; form-action 'none'",
        if connect.is_empty() {
            "'none'".to_owned()
        } else {
            connect
        }
    )
}

/// The plugin content origin: MCP App pages, and nothing else.
pub fn content_router(app: App) -> Router {
    Router::new()
        .route("/apps/{token}", get(page))
        .with_state(app)
}

async fn page(State(app): State<App>, Path(token): Path<String>) -> Response {
    let stored: Option<String> =
        sqlx::query_scalar("SELECT html FROM plugin_app_pages WHERE token=$1 AND expires_at>now()")
            .bind(&token)
            .fetch_optional(&app.store.pool)
            .await
            .ok()
            .flatten();
    let Some(stored) = stored.and_then(|s| serde_json::from_str::<Value>(&s).ok()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let csp = stored["csp"].as_str().unwrap_or("default-src 'none'");
    let mut response = stored["html"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
        .into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    if let Ok(csp) = HeaderValue::from_str(csp) {
        headers.insert("content-security-policy", csp);
    }
    response
}

#[derive(Deserialize)]
pub struct CallRequest {
    /// The tool item the app belongs to.
    item_id: Uuid,
    tool: String,
    #[serde(default)]
    arguments: Value,
}

/// `POST /orgs/{org}/conversations/{conv}/tool-calls`: a call an MCP App makes. It may call the
/// tools of the run its item belongs to, as the model could; tools that need the member's
/// approval each time are not callable from there.
pub async fn call(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Json(request): Json<CallRequest>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let (run, ..) = tool_call(&mut tx, org, ctx.user_id, conv, request.item_id).await?;
    let tool = super::run_tools(&app, &mut tx, org, run)
        .await?
        .into_iter()
        .find(|tool| tool.name == request.tool)
        .context(Code::ToolDenied)?;
    let policy = crate::store::effective_policy(&mut tx, &app.config().policy, org).await?;
    tx.commit().await?;
    require!(
        !policy
            .approval_required_tools
            .as_ref()
            .is_some_and(|patterns| ryzhub_domain::tool_matches(patterns, &tool.name)),
        Code::ToolDenied
    );
    let arguments = if request.arguments.is_null() {
        json!({})
    } else {
        request.arguments
    };
    require!(arguments.is_object(), Code::InvalidArguments);
    match super::call(&app, org, &tool, arguments, None).await? {
        super::client::Reply::Done(result) => Ok(Json(result)),
        super::client::Reply::InputRequired { .. } => Err(Code::PluginInputRequired.into()),
    }
}
