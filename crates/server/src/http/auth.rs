//! Accounts and sessions: registration, login, logout, WebSocket tickets.
use anyhow::Context;
use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng},
};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::{authenticate, user_orgs};
use crate::{
    app::App,
    error::{ApiResult, Code},
    store::{Store, hash_token, new_token},
};

const SESSION_HOURS: i64 = 12;
const TICKET_SECS: i64 = 30;
/// Failed logins allowed per email address in one window.
const LOGIN_ATTEMPTS: u32 = 10;
const LOGIN_WINDOW: Duration = Duration::from_secs(60);

/// Password hashing is deliberately expensive; bound how much of it runs at once.
static HASHING: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(std::thread::available_parallelism().map_or(2, |n| n.get())));
/// Failed attempts per email: (window start, count).
static FAILURES: LazyLock<Mutex<HashMap<String, (Instant, u32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Verified when the email is unknown, so response time does not reveal accounts.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    Argon2::default()
        .hash_password(new_token().as_bytes(), &SaltString::generate(&mut OsRng))
        .map(|hash| hash.to_string())
        .unwrap_or_default()
});

fn limited(email: &str) -> bool {
    let mut failures = FAILURES.lock().unwrap();
    let now = Instant::now();
    failures.retain(|_, (start, _)| now.duration_since(*start) < LOGIN_WINDOW);
    failures
        .get(email)
        .is_some_and(|(_, count)| *count >= LOGIN_ATTEMPTS)
}

fn record_failure(email: &str) {
    let mut failures = FAILURES.lock().unwrap();
    let entry = failures
        .entry(email.to_owned())
        .or_insert((Instant::now(), 0));
    entry.1 += 1;
}

async fn blocking_hash<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> anyhow::Result<T> {
    let _permit = HASHING.acquire().await?;
    Ok(tokio::task::spawn_blocking(work).await?)
}

#[derive(Deserialize)]
pub struct Credentials {
    email: String,
    password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
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
    let expires = chrono::Utc::now() + chrono::Duration::hours(SESSION_HOURS);
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

/// `POST /auth/register`: a user plus their personal organization.
pub async fn register(
    State(app): State<App>,
    Json(input): Json<Credentials>,
) -> ApiResult<Json<LoginResponse>> {
    let email = input.email.trim().to_lowercase();
    require!(
        (8..=256).contains(&input.password.len()) && email.contains('@') && email.len() <= 254,
        Code::InvalidInput
    );
    let password = blocking_hash(move || {
        Argon2::default()
            .hash_password(input.password.as_bytes(), &SaltString::generate(&mut OsRng))
            .map(|hash| hash.to_string())
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    })
    .await??;
    let user = Uuid::now_v7();
    let org = Uuid::now_v7();
    let mut tx = app.store.tx(org).await?;
    let inserted = sqlx::query(
        "INSERT INTO users(id,email,display_name,password_hash) VALUES($1,$2,$3,$4) ON CONFLICT(email) DO NOTHING",
    )
    .bind(user)
    .bind(&email)
    .bind(email.split('@').next().unwrap_or("User"))
    .bind(password)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    require!(inserted == 1, Code::InvalidInput);
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

/// `POST /auth/login`
pub async fn login(
    State(app): State<App>,
    Json(input): Json<Credentials>,
) -> ApiResult<Json<LoginResponse>> {
    require!(
        input.password.len() <= 256 && input.email.len() <= 254,
        Code::InvalidCredentials
    );
    let email = input.email.trim().to_lowercase();
    require!(!limited(&email), Code::RateLimited);
    let row = sqlx::query("SELECT id,password_hash FROM users WHERE email=$1 AND status='active'")
        .bind(&email)
        .fetch_optional(&app.store.pool)
        .await?;
    let (user, hash) = match &row {
        Some(row) => (
            Some(row.get::<Uuid, _>("id")),
            row.get::<String, _>("password_hash"),
        ),
        None => (None, DUMMY_HASH.clone()),
    };
    let password = input.password;
    let valid = blocking_hash(move || {
        PasswordHash::new(&hash).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        })
    })
    .await?;
    let user = match user {
        Some(user) if valid => user,
        _ => {
            record_failure(&email);
            return Err(Code::InvalidCredentials.into());
        }
    };
    let orgs = user_orgs(&app.store, user).await?;
    Ok(Json(
        issue_session(&app.store, user, orgs.first().map(|(id, _)| *id)).await?,
    ))
}

/// `GET /me`
pub async fn me(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let orgs: Vec<Value> = user_orgs(&app.store, auth.user_id)
        .await?
        .into_iter()
        .map(|(id, name)| json!({"id":id,"name":name}))
        .collect();
    Ok(Json(json!({
        "user_id": auth.user_id,
        "is_instance_admin": auth.is_instance_admin,
        "organizations": orgs,
    })))
}

/// `POST /auth/logout`: revokes this session; open connections close via the revocation notice.
pub async fn logout(State(app): State<App>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let auth = authenticate(&app, &headers).await?;
    sqlx::query("UPDATE auth_sessions SET revoked_at=now() WHERE id=$1")
        .bind(auth.session_id)
        .execute(&app.store.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /auth/ticket`: a one-use, short-lived credential for browser WebSockets.
pub async fn ticket(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let token = new_token();
    sqlx::query(
        "INSERT INTO ws_tickets(token_hash,session_id,expires_at) VALUES($1,$2,now()+($3::bigint*interval '1 second'))",
    )
    .bind(hash_token(&token))
    .bind(auth.session_id)
    .bind(TICKET_SECS)
    .execute(&app.store.pool)
    .await
    .context("ticket")?;
    Ok(Json(json!({"ticket":token,"expires_in":TICKET_SECS})))
}
