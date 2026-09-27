use crate::{config::Config, store::*};
use anyhow::Context;
use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng},
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{any, get, post},
};
use ryzhub_domain::{Action, Principal, Resource, Role, authorize};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

#[derive(Clone)]
pub struct App {
    pub store: Store,
    pub nats: async_nats::Client,
    pub config: Config,
}
pub struct ApiError(pub anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = self.0.to_string();
        let status = match code.as_str() {
            "UNAUTHORIZED" | "INVALID_CREDENTIALS" => StatusCode::UNAUTHORIZED,
            "FORBIDDEN" | "access denied" | "MODEL_DENIED" | "PLATFORM_CREDENTIAL_DENIED" => {
                StatusCode::FORBIDDEN
            }
            "INVALID_INPUT" | "INVALID_CONVERSATION" => StatusCode::BAD_REQUEST,
            "RATE_LIMITED" => StatusCode::TOO_MANY_REQUESTS,
            "FENCED" | "WORKSPACE_BUSY" | "WORKSPACE_RECOVERY_REQUIRED" => StatusCode::CONFLICT,
            "UPSTREAM_UNAVAILABLE" => StatusCode::BAD_GATEWAY,
            "CONVERSATION_BUSY" | "RUN_BUSY_USE_STEER" => StatusCode::CONFLICT,
            "MEMORY_MODE_IMMUTABLE"
            | "INVALID_PROJECT"
            | "INVALID_PROJECT_FILE"
            | "INVALID_DEFAULTS"
            | "INVALID_CONFIG"
            | "INVALID_HARNESS"
            | "INVALID_REASONING"
            | "INVALID_APPROVAL_MODE"
            | "INVALID_PROTOCOL"
            | "MODEL_NOT_ROUTABLE"
            | "PLAN_UNSUPPORTED"
            | "RESTORE_EXPIRED"
            | "PROJECT_DELETED"
            | "PROJECT_FILE_LIMIT"
            | "POLICY_DENIED" => StatusCode::BAD_REQUEST,
            "MODEL_GATEWAY_DISABLED" | "CREDENTIAL_UNAVAILABLE" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error=%self.0,"request failed");
        }
        (status,Json(json!({"error":if status==StatusCode::INTERNAL_SERVER_ERROR {"INTERNAL_ERROR"}else{&code}}))).into_response()
    }
}
pub type ApiResult<T> = Result<T, ApiError>;
pub fn router(app: App) -> Router {
    let origins: Vec<axum::http::HeaderValue> = app
        .config
        .allowed_origins
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    Router::new()
        .route(
            "/healthz",
            get(|State(app): State<App>| async move {
                Json(json!({"status":"ok","milestone":if !app.config.work.claude_model.is_empty() {"M3"} else if app.config.work.enabled {"M2"}else if app.config.llm.enabled {"M1"}else{"M0"}}))
            }),
        )
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/ticket", post(ticket))
        .route("/me", get(me))
        .route(
            "/orgs/{org}/conversations",
            get(list_conversations).post(create_conversation),
        )
        .route("/orgs/{org}/conversations/{conv}/items", get(history))
        .route(
            "/orgs/{org}/conversations/{conv}/model-credential",
            post(crate::llm::credentials::select),
        )
        .route(
            "/orgs/{org}/members/{user}",
            axum::routing::delete(remove_member),
        )
        .route("/orgs/{org}/policy", get(policy))
        .route("/orgs/{org}/models", get(models))
        .route("/orgs/{org}/audit", get(audit_log))
        .route(
            "/orgs/{org}/credentials",
            get(crate::llm::credentials::list).post(crate::llm::credentials::put),
        )
        .route(
            "/orgs/{org}/credentials/{id}",
            axum::routing::delete(crate::llm::credentials::revoke),
        )
        .route("/orgs/{org}/usage", get(crate::llm::gateway::usage))
        .route(
            "/orgs/{org}/conversations/{conv}/workspace",
            get(crate::checkpoint::status).post(crate::checkpoint::enqueue),
        )
        .route("/orgs/{org}/projects", get(crate::projects::list).post(crate::projects::create))
        .route("/orgs/{org}/projects/{project}", axum::routing::patch(crate::projects::update).delete(crate::projects::delete))
        .route("/orgs/{org}/projects/{project}/restore", post(crate::projects::restore))
        .route("/orgs/{org}/projects/{project}/files", get(crate::projects::files).post(crate::projects::put_file))
        .route("/orgs/{org}/projects/{project}/files/{file}", axum::routing::delete(crate::projects::remove_file))
        .route("/orgs/{org}/conversations/{conv}/project", post(crate::projects::move_conversation))
        .route("/orgs/{org}/conversations/{conv}/commands", get(crate::conversation_commands::catalog))
        .route("/orgs/{org}/conversations/{conv}/settings", axum::routing::patch(crate::conversation_commands::platform))
        .route("/orgs/{org}/conversations/{conv}/export", get(crate::conversation_commands::export))
        .route("/capabilities", get(capabilities))
        .route(
            "/admin",
            get(|| async { Html(include_str!("../../../docs/admin.html")) }),
        )
        .route("/admin/settings", get(settings))
        .route("/ws", any(crate::ws::upgrade))
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_origin(origins)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::PATCH,
                    axum::http::Method::DELETE,
                ])
                .allow_headers([
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::CONTENT_TYPE,
                ]),
        )
        .with_state(app)
}
async fn capabilities(State(app): State<App>) -> Json<Value> {
    let mut work_caps = ryzhub_execution::native::capabilities("codex");
    work_caps.as_object_mut().unwrap().extend(
        crate::run_config::capabilities("codex", "responses")
            .as_object()
            .unwrap()
            .clone(),
    );
    Json(
        json!({"chat":if app.config.llm.enabled {"anthropic"}else{"mock"},"model":if app.config.llm.enabled {app.config.llm.model.as_str()}else{"mock"},"work":if app.config.work.enabled {"codex"}else{"mock"},"work_model":app.config.work.model,"claude_model":app.config.work.claude_model,"responses_model":app.config.llm.responses_model,"harnesses":if app.config.work.claude_model.is_empty(){vec!["codex"]}else{vec!["codex","claude"]},"work_capabilities":if app.config.work.enabled {work_caps}else{json!({})},"milestone":if !app.config.work.claude_model.is_empty() {"M3"} else if app.config.work.enabled {"M2"}else if app.config.llm.enabled {"M1"}else{"M0"}}),
    )
}
pub async fn authenticate(app: &App, headers: &HeaderMap) -> anyhow::Result<Auth> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .context("UNAUTHORIZED")?;
    app.store.authenticate(token).await
}
#[derive(Deserialize)]
struct Credentials {
    email: String,
    password: String,
}
#[derive(Serialize)]
struct LoginResponse {
    token: String,
    user_id: Uuid,
    org_id: Option<Uuid>,
    expires_at_ms: i64,
}
async fn issue_session(
    store: &Store,
    user: Uuid,
    org: Option<Uuid>,
) -> anyhow::Result<LoginResponse> {
    let token = new_token();
    let expires = chrono::Utc::now() + chrono::Duration::hours(12);
    sqlx::query("INSERT INTO auth_sessions(id,user_id,token_hash,expires_at) VALUES($1,$2,$3,$4)")
        .bind(Uuid::now_v7())
        .bind(user)
        .bind(hash_token(&token))
        .bind(expires)
        .execute(&store.pool)
        .await?;
    Ok(LoginResponse {
        token,
        user_id: user,
        org_id: org,
        expires_at_ms: expires.timestamp_millis(),
    })
}
async fn register(
    State(app): State<App>,
    Json(input): Json<Credentials>,
) -> ApiResult<Json<LoginResponse>> {
    let email = input.email.trim().to_lowercase();
    check(
        (8..=256).contains(&input.password.len()) && email.contains('@') && email.len() <= 254,
        "INVALID_INPUT",
    )?;
    let password = tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(input.password.as_bytes(), &SaltString::generate(&mut OsRng))
            .map(|p| p.to_string())
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    })
    .await??;
    let user = Uuid::now_v7();
    let org = Uuid::now_v7();
    let mut tx = app.store.tx(org).await?;
    let inserted=sqlx::query("INSERT INTO users(id,email,display_name,password_hash) VALUES($1,$2,$3,$4) ON CONFLICT(email) DO NOTHING").bind(user).bind(&email).bind(email.split('@').next().unwrap_or("User")).bind(password).execute(&mut *tx).await?.rows_affected();
    check(inserted == 1, "INVALID_INPUT")?;
    sqlx::query("INSERT INTO organizations(id,kind,name) VALUES($1,'personal','个人空间')")
        .bind(org)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO memberships(org_id,user_id,role) VALUES($1,$2,'owner')")
        .bind(org)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO org_policies(org_id,version,policy) VALUES($1,1,'{}')")
        .bind(org)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO channels(id,org_id,kind,user_id) VALUES($1,$2,'user',$3),($4,$2,'org',NULL)",
    )
    .bind(format!("user:{org}:{user}"))
    .bind(org)
    .bind(user)
    .bind(format!("org:{org}"))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(issue_session(&app.store, user, Some(org)).await?))
}
async fn login(
    State(app): State<App>,
    Json(input): Json<Credentials>,
) -> ApiResult<Json<LoginResponse>> {
    check(
        input.password.len() <= 256 && input.email.len() <= 254,
        "INVALID_CREDENTIALS",
    )?;
    let row = sqlx::query("SELECT id,password_hash FROM users WHERE email=$1 AND status='active'")
        .bind(input.email.trim().to_lowercase())
        .fetch_optional(&app.store.pool)
        .await?
        .context("INVALID_CREDENTIALS")?;
    let hash: String = row.get("password_hash");
    let user: Uuid = row.get("id");
    let valid = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).is_ok_and(|h| {
            Argon2::default()
                .verify_password(input.password.as_bytes(), &h)
                .is_ok()
        })
    })
    .await?;
    check(valid, "INVALID_CREDENTIALS")?;
    let orgs = user_orgs(&app.store, user).await?;
    Ok(Json(
        issue_session(&app.store, user, orgs.first().map(|(id, _)| *id)).await?,
    ))
}
async fn user_orgs(store: &Store, user: Uuid) -> anyhow::Result<Vec<(Uuid, String)>> {
    Ok(sqlx::query_as("SELECT id,name FROM list_user_orgs($1)")
        .bind(user)
        .fetch_all(&store.pool)
        .await?)
}
async fn me(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let orgs = user_orgs(&app.store, auth.user_id).await?;
    Ok(Json(
        json!({"user_id":auth.user_id,"is_instance_admin":auth.is_instance_admin,"organizations":orgs.into_iter().map(|(id,name)|json!({"id":id,"name":name})).collect::<Vec<_>>()}),
    ))
}
async fn logout(State(app): State<App>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let auth = authenticate(&app, &headers).await?;
    sqlx::query("UPDATE auth_sessions SET revoked_at=now() WHERE id=$1")
        .bind(auth.session_id)
        .execute(&app.store.pool)
        .await?;
    app.nats
        .publish(format!("ryzhub.revoke.{}", auth.user_id), "revoked".into())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn ticket(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let token = new_token();
    sqlx::query("DELETE FROM ws_tickets WHERE expires_at<now()")
        .execute(&app.store.pool)
        .await?;
    sqlx::query("INSERT INTO ws_tickets(token_hash,session_id,expires_at) VALUES($1,$2,now()+interval '30 seconds')").bind(hash_token(&token)).bind(auth.session_id).execute(&app.store.pool).await?;
    Ok(Json(json!({"ticket":token,"expires_in":30})))
}
#[derive(Deserialize)]
struct CreateConversation {
    title: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    project_id: Option<Uuid>,
    #[serde(default = "empty_config")]
    run_config: Value,
}
fn empty_config() -> Value {
    json!({})
}
fn default_mode() -> String {
    String::new()
}
async fn create_conversation(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(input): Json<CreateConversation>,
) -> ApiResult<Json<Conversation>> {
    let auth = authenticate(&app, &headers).await?;
    let ctx = app.store.context(auth.user_id, org).await?;
    Ok(Json(
        app.store
            .create_configured(
                &ctx,
                &input.title,
                &input.mode,
                input.run_config,
                input.project_id,
                Some(&app.config),
            )
            .await?,
    ))
}
async fn list_conversations(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<Vec<Conversation>>> {
    let auth = authenticate(&app, &headers).await?;
    let ctx = app.store.context(auth.user_id, org).await?;
    Ok(Json(app.store.list_conversations(&ctx).await?))
}
#[derive(Deserialize)]
struct HistoryQuery {
    before_seq: Option<i64>,
}
async fn history(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Query(query): Query<HistoryQuery>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let ctx = app.store.context(auth.user_id, org).await?;
    let (items, more) = app
        .store
        .history(&ctx, conv, query.before_seq.unwrap_or(i64::MAX))
        .await?;
    // `has_more` lets clients stop paging without guessing from page size.
    Ok(Json(json!({"items":items,"has_more":more})))
}
async fn policy(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<ryzhub_domain::Policy>> {
    let auth = authenticate(&app, &headers).await?;
    let ctx = app.store.context(auth.user_id, org).await?;
    let mut tx = app.store.tx(ctx.org_id).await?;
    let policy: Option<Value> = sqlx::query_scalar(
        "SELECT policy FROM org_policies WHERE org_id=$1 ORDER BY version DESC LIMIT 1",
    )
    .bind(org)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(app.config.policy.restrict(&serde_json::from_value(
        policy.unwrap_or(json!({})),
    )?)))
}
/// Models a member may start a conversation with, grouped the way clients pick
/// them: Chat by model, Work by harness and then model. The wire protocol stays
/// server-side; it follows each model's provider.
async fn models(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let ctx = app.store.context(auth.user_id, org).await?;
    let mut tx = app.store.tx(ctx.org_id).await?;
    let policy = crate::llm::gateway::effective_policy(&app, &mut tx, org).await?;
    tx.commit().await?;
    let config = &app.config;
    let catalog = crate::run_config::catalog(config);
    let list = |harness: &str| -> Vec<Value> {
        let default = match harness {
            _ if !config.llm.enabled => "mock",
            "codex" => config.work.model.as_str(),
            "claude" => config.work.claude_model.as_str(),
            _ => config.llm.model.as_str(),
        };
        let mut models: Vec<&crate::run_config::ModelEntry> = catalog
            .iter()
            .filter(|m| m.harnesses.iter().any(|h| h == harness))
            .filter(|m| {
                policy.models.as_ref().is_none_or(|p| p.contains(&m.id))
                    && policy
                        .harnesses
                        .as_ref()
                        .is_none_or(|p| p.contains(harness))
            })
            .collect();
        // Stable sort: the default leads, so pickers open on it.
        models.sort_by_key(|m| m.id != default);
        models
            .into_iter()
            .map(|m| {
                let (provider, levels) = if config.llm.enabled {
                    (
                        crate::run_config::provider(&m.protocol),
                        crate::run_config::reasoning_levels(harness, &m.protocol),
                    )
                } else {
                    ("mock", vec![])
                };
                json!({"id":m.id,"provider":provider,"default":m.id==default,"reasoning_levels":levels})
            })
            .collect()
    };
    let harnesses: &[&str] = if !config.llm.enabled {
        &["codex"]
    } else if config.work.enabled {
        &["codex", "claude"]
    } else {
        &[]
    };
    let work: Vec<Value> = harnesses
        .iter()
        .map(|h| (h, list(h)))
        .filter(|(_, models)| !models.is_empty())
        .map(|(h, models)| json!({"harness":h,"models":models}))
        .collect();
    Ok(Json(json!({"chat":list("chat"),"work":work})))
}
async fn audit_log(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<Vec<Value>>> {
    let auth = authenticate(&app, &headers).await?;
    let ctx = app.store.context(auth.user_id, org).await?;
    authorize(
        &Principal::User(ctx),
        Action::ViewAudit,
        &Resource {
            org_id: org,
            id: org,
            owner_user_id: None,
            target_role: None,
        },
    )?;
    let mut tx = app.store.tx(org).await?;
    let rows:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(a) FROM (SELECT id,actor_id,action,resource,result,created_at FROM audit_logs WHERE org_id=$1 ORDER BY created_at DESC LIMIT 100) a").bind(org).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(rows))
}
async fn remove_member(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, user)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let auth = authenticate(&app, &headers).await?;
    let ctx = app.store.context(auth.user_id, org).await?;
    let target = app.store.context(user, org).await?;
    authorize(
        &Principal::User(ctx.clone()),
        Action::ManageMember,
        &Resource {
            org_id: org,
            id: user,
            owner_user_id: None,
            target_role: Some(target.role),
        },
    )?;
    check(target.role != Role::Owner, "FORBIDDEN")?;
    let mut tx = app.store.tx(org).await?;
    sqlx::query("DELETE FROM memberships WHERE org_id=$1 AND user_id=$2")
        .bind(org)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE auth_sessions SET revoked_at=now() WHERE user_id=$1")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE virtual_keys SET revoked_at=now() WHERE org_id=$1 AND user_id=$2")
        .bind(org)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &ctx, "member.remove", &user.to_string(), "allowed").await?;
    tx.commit().await?;
    app.nats
        .publish(format!("ryzhub.revoke.{user}"), "membership_removed".into())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn settings(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    check(auth.is_instance_admin, "FORBIDDEN")?;
    Ok(Json(
        json!({"config":app.config,"backend":if app.config.sandbox.enabled {"containerd"}else{"disabled"},"isolation_level":if !app.config.sandbox.enabled {"none"}else if app.config.sandbox.runtime=="runsc" {"user_kernel"}else{"container"},"milestone":if !app.config.work.claude_model.is_empty() {"M3"} else if app.config.work.enabled {"M2"}else if app.config.llm.enabled {"M1"}else{"M0"},"capabilities":{"stream":"text/0","approval":"native","resume":if app.config.llm.enabled {"loop_steps"}else{"emulated"},"chat":if app.config.llm.enabled {"anthropic"}else{"mock"},"work":if app.config.work.enabled {"codex"}else{"mock"},"pty":false}}),
    ))
}

pub fn check(condition: bool, code: &str) -> ApiResult<()> {
    if condition {
        Ok(())
    } else {
        Err(ApiError(anyhow::anyhow!(code.to_owned())))
    }
}
