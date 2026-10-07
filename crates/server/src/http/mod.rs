//! The public HTTP API: REST resources and the WebSocket endpoint.
mod auth;
mod orgs;

use anyhow::Context;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Method, header},
    response::Html,
    routing::{any, delete, get, patch, post},
};
use serde_json::{Value, json};
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use crate::{
    app::App,
    error::{ApiResult, Code},
    store::{Auth, Store, Tx, context_in},
};
use ryzhub_domain::OrgContext;

pub fn router(app: App) -> Router {
    use crate::{
        attachments::http as attachments, conversation_commands as conversations, llm, memory,
        projects, search, workspace,
    };
    let origins: Vec<axum::http::HeaderValue> = app
        .config
        .allowed_origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        // The refresh cookie travels with the web client's `/auth` requests.
        .allow_credentials(true);
    Router::new()
        .route("/orgs/{org}/search", get(search::endpoint))
        .route(
            "/orgs/{org}/attachments",
            get(attachments::list).post(attachments::upload),
        )
        .route("/orgs/{org}/attachments/{id}", delete(attachments::remove))
        .route(
            "/orgs/{org}/attachments/{id}/content",
            get(attachments::content),
        )
        .route(
            "/orgs/{org}/attachments/{id}/preview",
            get(attachments::preview),
        )
        .route("/orgs/{org}/memories", get(memory::list).post(memory::put))
        .route(
            "/orgs/{org}/memories/preferences",
            patch(memory::set_preferences),
        )
        .route("/orgs/{org}/memories/{id}", delete(memory::remove))
        .route("/orgs/{org}/memory-events/{id}/undo", post(memory::revert))
        .route(
            "/orgs/{org}/conversations/{conv}/memory-context/{item}",
            get(memory::timeline_context),
        )
        .route(
            "/orgs/{org}/conversations/{conv}",
            delete(memory::delete_conversation),
        )
        .route("/healthz", get(healthz))
        .route("/capabilities", get(capabilities))
        .route(
            "/admin",
            get(|| async { Html(include_str!("../../../../docs/admin.html")) }),
        )
        .route("/admin/settings", get(admin_settings))
        .route("/ws", any(crate::ws::upgrade))
        .route("/auth/register", post(auth::register))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/refresh", post(auth::refresh))
        .route("/auth/ticket", post(auth::ticket))
        .route("/me", get(auth::me))
        .route("/orgs/{org}/policy", get(orgs::policy))
        .route("/orgs/{org}/models", get(orgs::models))
        .route("/orgs/{org}/audit", get(orgs::audit_log))
        .route("/orgs/{org}/usage", get(llm::gateway::usage))
        .route("/orgs/{org}/members/{user}", delete(orgs::remove_member))
        .route(
            "/orgs/{org}/credentials",
            get(llm::credentials::list).post(llm::credentials::put),
        )
        .route(
            "/orgs/{org}/credentials/{id}",
            delete(llm::credentials::revoke),
        )
        .route(
            "/orgs/{org}/conversations",
            get(orgs::list_conversations).post(orgs::create_conversation),
        )
        .route("/orgs/{org}/conversations/{conv}/items", get(orgs::history))
        .route(
            "/orgs/{org}/conversations/{conv}/model-credential",
            post(llm::credentials::select),
        )
        .route(
            "/orgs/{org}/conversations/{conv}/workspace",
            get(workspace::status).post(workspace::enqueue),
        )
        .route(
            "/orgs/{org}/conversations/{conv}/project",
            post(projects::move_conversation),
        )
        .route(
            "/orgs/{org}/conversations/{conv}/commands",
            get(conversations::catalog),
        )
        .route(
            "/orgs/{org}/conversations/{conv}/settings",
            patch(conversations::platform),
        )
        .route(
            "/orgs/{org}/conversations/{conv}/export",
            get(conversations::export),
        )
        .route(
            "/orgs/{org}/projects",
            get(projects::list).post(projects::create),
        )
        .route(
            "/orgs/{org}/projects/{project}",
            patch(projects::update).delete(projects::delete),
        )
        .route(
            "/orgs/{org}/projects/{project}/restore",
            post(projects::restore),
        )
        .route(
            "/orgs/{org}/projects/{project}/files",
            get(projects::files).post(projects::put_file),
        )
        .route(
            "/orgs/{org}/projects/{project}/files/{file}",
            delete(projects::remove_file),
        )
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(cors)
        .with_state(app)
}

/// The session behind the request's bearer token.
pub async fn authenticate(app: &App, headers: &HeaderMap) -> anyhow::Result<Auth> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .context(Code::Unauthorized)?;
    app.store.authenticate(token).await
}

/// The caller's membership in `org`.
pub async fn member(app: &App, headers: &HeaderMap, org: Uuid) -> anyhow::Result<OrgContext> {
    let auth = authenticate(app, headers).await?;
    app.store.context(auth.user_id, org).await
}

/// The caller's membership in `org`, checked inside a new transaction scoped to `org`, so the
/// rest of the request runs under the membership it was authorized with.
pub async fn member_tx<'a>(
    app: &'a App,
    headers: &HeaderMap,
    org: Uuid,
) -> anyhow::Result<(OrgContext, Tx<'a>)> {
    let auth = authenticate(app, headers).await?;
    let mut tx = app.store.tx(org).await?;
    let ctx = context_in(&mut tx, auth.user_id, org).await?;
    Ok((ctx, tx))
}

pub(crate) async fn user_orgs(store: &Store, user: Uuid) -> anyhow::Result<Vec<(Uuid, String)>> {
    Ok(sqlx::query_as("SELECT id,name FROM list_user_orgs($1)")
        .bind(user)
        .fetch_all(&store.pool)
        .await?)
}

fn milestone(app: &App) -> &'static str {
    let config = &app.config;
    if config.search.enabled {
        "M4"
    } else if !config.work.claude_model.is_empty() {
        "M3"
    } else if config.work.enabled {
        "M2"
    } else if config.llm.enabled {
        "M1"
    } else {
        "M0"
    }
}

/// Liveness plus the database, which every request needs.
async fn healthz(State(app): State<App>) -> ApiResult<Json<Value>> {
    sqlx::query("SELECT 1").execute(&app.store.pool).await?;
    Ok(Json(json!({"status":"ok","milestone":milestone(&app)})))
}

async fn capabilities(State(app): State<App>) -> Json<Value> {
    let config = &app.config;
    let mut work = ryzhub_execution::native::capabilities("codex");
    if let (Some(work), Some(extra)) = (
        work.as_object_mut(),
        crate::run_config::capabilities("codex", "responses").as_object(),
    ) {
        work.extend(extra.clone());
    }
    let harnesses = if config.work.claude_model.is_empty() {
        vec!["codex"]
    } else {
        vec!["codex", "claude"]
    };
    Json(json!({
        "chat": if config.llm.enabled { crate::run_config::default_chat_protocol(config) } else { "mock" },
        "model": if config.llm.enabled { config.llm.model.as_str() } else { "mock" },
        "work": if config.work.enabled { "codex" } else { "mock" },
        "work_model": config.work.model,
        "claude_model": config.work.claude_model,
        "responses_model": config.llm.responses_model,
        "harnesses": harnesses,
        "work_capabilities": if config.work.enabled { work } else { json!({}) },
        "milestone": milestone(&app),
        "search": config.search.enabled,
        "memory": config.search.enabled,
        "conversation_temporary": config.search.enabled,
        "conversation_delete": config.search.enabled,
        "conversation_pin": true,
        "attachments": {
            "max_bytes": config.policy.attachment_bytes.unwrap_or(crate::attachments::DEFAULT_BYTES)
                .min(crate::attachments::MAX_BYTES),
            "max_per_message": crate::attachments::MAX_PER_MESSAGE,
        },
    }))
}

/// `GET /admin/settings`: the effective instance configuration, for instance admins.
async fn admin_settings(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    require!(auth.is_instance_admin, Code::Forbidden);
    let config = &app.config;
    let isolation = match (config.sandbox.enabled, config.sandbox.runtime.as_str()) {
        (false, _) => "none",
        (true, "runsc") => "user_kernel",
        (true, _) => "container",
    };
    Ok(Json(json!({
        "config": config,
        "backend": if config.sandbox.enabled { "containerd" } else { "disabled" },
        "isolation_level": isolation,
        "milestone": milestone(&app),
        "capabilities": {
            "stream": "text/0",
            "approval": "native",
            "resume": if config.llm.enabled { "loop_steps" } else { "emulated" },
            "chat": if config.llm.enabled { "anthropic" } else { "mock" },
            "work": if config.work.enabled { "codex" } else { "mock" },
            "pty": false,
        },
    })))
}
