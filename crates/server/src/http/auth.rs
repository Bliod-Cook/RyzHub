//! Accounts and sessions: registration, login, refresh, logout, WebSocket tickets.
//!
//! An access token lives only in the client's memory. Login also signs in the device: its
//! refresh token goes to the browser as an HttpOnly cookie scoped to `/auth`, and
//! `/auth/refresh` trades it for a new access token after a reload.
use anyhow::Context;
use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng},
};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
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
/// A device stays signed in this long after its last refresh.
const REFRESH_DAYS: i64 = 30;
/// A rotated refresh token still works this long, for tabs that refreshed at the same time.
const ROTATION_GRACE_SECS: i64 = 60;
const REFRESH_COOKIE: &str = "ryzhub_refresh";
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
    email: String,
    expires_at_ms: i64,
}

/// An access token for `user`, issued through `device` when it came from a refresh.
async fn issue_session(
    conn: &mut sqlx::PgConnection,
    user: Uuid,
    org: Option<Uuid>,
    device: Option<Uuid>,
) -> anyhow::Result<LoginResponse> {
    let token = new_token();
    let expires = chrono::Utc::now() + chrono::Duration::hours(SESSION_HOURS);
    sqlx::query(
        "INSERT INTO auth_sessions(id,user_id,token_hash,expires_at,device_id) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(Uuid::now_v7())
    .bind(user)
    .bind(hash_token(&token))
    .bind(expires)
    .bind(device)
    .execute(&mut *conn)
    .await?;
    let email = sqlx::query_scalar("SELECT email FROM users WHERE id=$1")
        .bind(user)
        .fetch_one(&mut *conn)
        .await?;
    Ok(LoginResponse {
        token,
        user_id: user,
        org_id: org,
        email,
        expires_at_ms: expires.timestamp_millis(),
    })
}

/// The `Set-Cookie` value that stores a device's refresh token, or clears it when `max_age` is
/// zero. Scoped to `/auth` and hidden from scripts; `Secure` whenever the page is served over
/// HTTPS.
fn refresh_cookie(headers: &HeaderMap, token: &str, max_age: i64) -> String {
    let https = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|origin| origin.starts_with("https://"))
        || headers
            .get("x-forwarded-proto")
            .is_some_and(|proto| proto == "https");
    format!(
        "{REFRESH_COOKIE}={token}; Path=/auth; Max-Age={max_age}; HttpOnly; SameSite=Strict{}",
        if https { "; Secure" } else { "" }
    )
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_owned())
}

/// Sign in a device: an access token, and the cookie that renews it.
async fn sign_in(
    store: &Store,
    headers: &HeaderMap,
    user: Uuid,
    org: Option<Uuid>,
) -> ApiResult<impl IntoResponse + use<>> {
    let refresh = new_token();
    let device = Uuid::now_v7();
    let mut tx = store.pool.begin().await?;
    sqlx::query(
        "INSERT INTO auth_refresh(id,user_id,refresh_hash,expires_at)
         VALUES($1,$2,$3,now()+($4::bigint*interval '1 day'))",
    )
    .bind(device)
    .bind(user)
    .bind(hash_token(&refresh))
    .bind(REFRESH_DAYS)
    .execute(&mut *tx)
    .await?;
    let session = issue_session(&mut tx, user, org, Some(device)).await?;
    tx.commit().await?;
    Ok((
        [(
            header::SET_COOKIE,
            refresh_cookie(headers, &refresh, REFRESH_DAYS * 86400),
        )],
        Json(session),
    ))
}

/// Revoke a device and every access token issued through it; open connections close via the
/// revocation notice.
async fn revoke_device(conn: &mut sqlx::PgConnection, device: Uuid) -> anyhow::Result<()> {
    sqlx::query("UPDATE auth_refresh SET revoked_at=now() WHERE id=$1 AND revoked_at IS NULL")
        .bind(device)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE auth_sessions SET revoked_at=now() WHERE device_id=$1 AND revoked_at IS NULL",
    )
    .bind(device)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `POST /auth/register`: a user plus their personal organization.
pub async fn register(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<Credentials>,
) -> ApiResult<impl IntoResponse> {
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
    sign_in(&app.store, &headers, user, Some(org)).await
}

/// `POST /auth/login`
pub async fn login(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<Credentials>,
) -> ApiResult<impl IntoResponse> {
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
    sign_in(&app.store, &headers, user, orgs.first().map(|(id, _)| *id)).await
}

/// `POST /auth/refresh`: trade the device's refresh cookie for a new access token. The refresh
/// token rotates; the one just replaced still works briefly, and is a replay after that.
pub async fn refresh(State(app): State<App>, headers: HeaderMap) -> ApiResult<impl IntoResponse> {
    let presented = cookie(&headers, REFRESH_COOKIE).ok_or(Code::Unauthorized)?;
    let hash = hash_token(&presented);
    let mut tx = app.store.pool.begin().await?;
    let device: Option<(Uuid, Uuid, bool, bool)> = sqlx::query_as(
        "SELECT r.id,r.user_id,r.refresh_hash=$1,r.rotated_at>now()-($2::bigint*interval '1 second')
         FROM auth_refresh r JOIN users u ON u.id=r.user_id
         WHERE (r.refresh_hash=$1 OR r.previous_hash=$1) AND r.revoked_at IS NULL
           AND r.expires_at>now() AND u.status='active'
         FOR UPDATE OF r",
    )
    .bind(&hash)
    .bind(ROTATION_GRACE_SECS)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((device, user, current, recent)) = device else {
        return Err(Code::Unauthorized.into());
    };
    let rotated = if current {
        let next = new_token();
        sqlx::query(
            "UPDATE auth_refresh SET previous_hash=refresh_hash,refresh_hash=$2,rotated_at=now(),
                 expires_at=now()+($3::bigint*interval '1 day')
             WHERE id=$1",
        )
        .bind(device)
        .bind(hash_token(&next))
        .bind(REFRESH_DAYS)
        .execute(&mut *tx)
        .await?;
        Some(next)
    } else if recent {
        // Another tab rotated it a moment ago; the browser already holds the new cookie.
        None
    } else {
        // A replaced token presented again: whoever holds it, the device is no longer trusted.
        revoke_device(&mut tx, device).await?;
        tx.commit().await?;
        return Err(Code::Unauthorized.into());
    };
    let orgs = user_orgs(&app.store, user).await?;
    let session =
        issue_session(&mut tx, user, orgs.first().map(|(id, _)| *id), Some(device)).await?;
    tx.commit().await?;
    let cookie = rotated.map(|token| {
        (
            header::SET_COOKIE,
            refresh_cookie(&headers, &token, REFRESH_DAYS * 86400),
        )
    });
    Ok((axum::response::AppendHeaders(cookie), Json(session)))
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

/// `POST /auth/logout`: signs the device out: this session, the device's refresh token and every
/// access token it issued. Open connections close via the revocation notice.
pub async fn logout(State(app): State<App>, headers: HeaderMap) -> ApiResult<impl IntoResponse> {
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.pool.begin().await?;
    let device: Option<Uuid> = sqlx::query_scalar(
        "UPDATE auth_sessions SET revoked_at=now() WHERE id=$1 RETURNING device_id",
    )
    .bind(auth.session_id)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(device) = device {
        revoke_device(&mut tx, device).await?;
    }
    tx.commit().await?;
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, refresh_cookie(&headers, "", 0))],
    ))
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
