//! The public HTTP API: REST resources and the WebSocket endpoint.
mod auth;
mod orgs;
pub mod sso;

use anyhow::Context;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Method, header},
    response::Html,
    routing::{any, delete, get, patch, post, put},
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
        attachments::http as attachments, attachments::uploads,
        conversation_commands as conversations, llm, memory, projects, search, workspace,
    };
    let origins: Vec<axum::http::HeaderValue> = app
        .config
        .allowed_origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        // The refresh cookie travels with the web client's `/auth` requests.
        .allow_credentials(true);
    use crate::plugins::{api as plugins, oauth};
    Router::new()
        .route(
            "/orgs/{org}/plugins",
            get(plugins::list).post(plugins::install),
        )
        .route("/orgs/{org}/plugins/catalog", get(plugins::catalog))
        .route(
            "/orgs/{org}/plugins/preview",
            post(plugins::preview).layer(axum::extract::DefaultBodyLimit::max(24 * 1024 * 1024)),
        )
        .route(
            "/orgs/{org}/plugins/{id}",
            patch(plugins::change).delete(plugins::remove),
        )
        .route("/orgs/{org}/plugins/{id}/update", post(plugins::update))
        .route("/orgs/{org}/plugins/{id}/confirm", post(plugins::confirm))
        .route("/orgs/{org}/plugins/{id}/refresh", post(plugins::refresh))
        .route("/orgs/{org}/plugins/{id}/connect", post(plugins::connect))
        .route(
            "/orgs/{org}/plugins/{id}/connections/{server}",
            delete(plugins::disconnect),
        )
        .route(
            "/orgs/{org}/conversations/{conv}/plugins",
            get(plugins::conversation_plugins),
        )
        .route(
            "/orgs/{org}/conversations/{conv}/plugins/{id}",
            put(plugins::conversation_toggle),
        )
        .route(
            "/orgs/{org}/projects/{project}/plugins/{id}",
            put(plugins::project_toggle),
        )
        .route(
            "/orgs/{org}/conversations/{conv}/apps",
            post(crate::plugins::apps::open),
        )
        .route(
            "/orgs/{org}/conversations/{conv}/tool-calls",
            post(crate::plugins::apps::call),
        )
        .route(
            "/admin/plugin-blocks",
            get(plugins::blocks).post(plugins::block),
        )
        .route("/admin/plugin-blocks/{id}", delete(plugins::unblock))
        .route("/plugins/oauth/client.json", get(oauth::client_document))
        .route("/plugins/oauth/callback", get(oauth::callback))
        .route("/orgs/{org}/search", get(search::endpoint))
        .route(
            "/orgs/{org}/attachments",
            get(attachments::list).post(attachments::upload),
        )
        .route("/orgs/{org}/attachments/{id}", delete(attachments::remove))
        .route("/devices", post(crate::push::register))
        .route("/devices/{device}", delete(crate::push::unregister))
        .route("/orgs/{org}/uploads", post(uploads::start))
        .route(
            "/orgs/{org}/uploads/{id}",
            get(uploads::show).delete(uploads::abort),
        )
        .route("/orgs/{org}/uploads/{id}/parts/{n}", put(uploads::part))
        .route(
            "/orgs/{org}/uploads/{id}/parts/{n}/sent",
            post(uploads::sent),
        )
        .route("/orgs/{org}/uploads/{id}/complete", post(uploads::complete))
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
        .route(
            "/orgs/{org}/conversations/{conv}/memory-context/{item}",
            get(memory::timeline_context),
        )
        .route(
            "/orgs/{org}/conversations/{conv}",
            delete(conversations::delete),
        )
        .route("/healthz", get(healthz))
        .route("/capabilities", get(capabilities))
        .route("/admin", get(|| async { Html(include_str!("admin.html")) }))
        .route("/admin/settings", get(admin_settings))
        .route("/ws", any(crate::ws::upgrade))
        .route("/stream", get(crate::ws::sse::open))
        .route("/stream/{connection}", post(crate::ws::sse::post))
        .route("/auth/oauth/{provider}/start", get(sso::start))
        .route("/auth/oauth/{provider}/callback", get(sso::callback))
        .route("/auth/oauth/exchange", post(sso::exchange))
        .route("/me/identities", get(sso::identities))
        .route("/me/identities/{provider}/{subject}", delete(sso::unlink))
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
        .route("/orgs/{org}/conversations", get(orgs::list_conversations))
        .route("/orgs/{org}/conversations/{conv}/items", get(orgs::history))
        .route(
            "/orgs/{org}/conversations/{conv}/workspace",
            get(workspace::status).post(workspace::enqueue),
        )
        .route(
            "/orgs/{org}/conversations/{conv}/terminal",
            any(crate::terminal::upgrade),
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
            "/orgs/{org}/conversations/{conv}/draft",
            put(crate::drafts::put),
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
        .route(
            "/orgs/{org}/projects/{project}/files/{file}/source",
            get(projects::file_source),
        )
        .route(
            "/orgs/{org}/projects/{project}/files/upload",
            post(projects::upload_file),
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

/// Liveness plus the database, which every request needs.
async fn healthz(State(app): State<App>) -> ApiResult<Json<Value>> {
    sqlx::query("SELECT 1").execute(&app.store.pool).await?;
    Ok(Json(json!({"status":"ok"})))
}

async fn capabilities(State(app): State<App>) -> Json<Value> {
    let config = &app.config;
    let harnesses = crate::run_config::work_harnesses(config);
    // Null where the engine is off.
    Json(json!({
        "chat": config.llm.enabled.then(|| crate::run_config::default_chat_protocol(config)),
        "model": config.llm.enabled.then_some(&config.llm.model),
        "work": config.work.enabled.then_some("codex"),
        "work_model": config.work.model,
        "claude_model": config.work.claude_model,
        "harnesses": harnesses,
        "search": config.search.enabled,
        "memory": config.search.enabled,
        "conversation_temporary": config.search.enabled,
        "conversation_delete": config.search.enabled,
        // Sign-in with other accounts: `/auth/oauth/<id>/start?return_to=`.
        "auth_providers": sso::providers(&app),
        // Plugins, their commands (InvokeMenuCommand) and questions (PendingAction.form_json),
        // MCP Apps from `plugin_content_origin`.
        "plugins": config.plugins.enabled,
        // Android notifications: `POST /devices` with an FCM token.
        "push": config.push.enabled,
        "plugin_content_origin": config.plugins.content_origin,
        "attachments": {
            "max_bytes": crate::attachments::limit(&config.policy),
            "max_per_message": crate::attachments::MAX_PER_MESSAGE,
            // Larger files go in parts of this size through `/orgs/{org}/uploads`.
            "part_bytes": crate::attachments::uploads::PART_SIZE,
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
        "capabilities": {
            "stream": "text/0",
            "approval": "native",
            "resume": "loop_steps",
            "chat": config.llm.enabled.then_some("anthropic"),
            "work": config.work.enabled.then_some("codex"),
            "terminal": config.work.enabled && config.sandbox.enabled,
        },
    })))
}
