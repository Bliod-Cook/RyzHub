//! Notifications to Android devices through Firebase Cloud Messaging (HTTP v1).
//!
//! An approval or question waiting, and a run that ended, enqueue a push job in the
//! transaction that caused them (the jobs are the outbox). The job notifies the conversation's
//! creator on each registered device of a sign-in still valid, except devices with a live
//! connection: they already show it. A token FCM no longer knows is forgotten.
//!
//! The Owner role's [`worker`] claims due jobs with `FOR UPDATE SKIP LOCKED` and retries
//! failures with exponential backoff until [`MAX_ATTEMPTS`], after which a job is `dead`.
//! Delivery is at least once: a delivery that outlives its lease may run again elsewhere (a
//! device notified twice shows one notification). Completion is fenced by the claim token.
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::Context;
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    app::App,
    error::{ApiResult, Code},
    store::Tx,
};

/// A device stays online this long after its connection last showed it alive.
const ONLINE_SECS: i64 = 60;
/// Deliveries a notification gets.
pub const MAX_ATTEMPTS: i32 = 5;
/// A delivery is cut off after this long, and its job may be claimed again.
const LEASE: Duration = Duration::from_secs(60);
/// Idle wait between scans for due jobs.
const IDLE: Duration = Duration::from_secs(1);
/// How often finished jobs are swept.
const SWEEP_EVERY: Duration = Duration::from_secs(600);
/// Jobs claimed per organization per scan, so one busy organization cannot starve others.
const PER_ORG: usize = 32;

#[derive(Deserialize)]
pub struct Register {
    /// The client's own device id, as in its Hello.
    device: String,
    platform: String,
    token: String,
    #[serde(default)]
    locale: String,
}

/// `POST /devices`: notify this signed-in device at `token`. Registering again (a new token,
/// another account) replaces what the device or the token had.
pub async fn register(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<Register>,
) -> ApiResult<Json<Value>> {
    require!(app.config().push.enabled, Code::InvalidInput);
    require!(
        input.platform == "android"
            && !input.device.is_empty()
            && input.device.len() <= 128
            && !input.token.is_empty()
            && input.token.len() <= 4096,
        Code::InvalidInput
    );
    let auth = crate::http::authenticate(&app, &headers).await?;
    let mut tx = app.store.pool.begin().await?;
    let sign_in: Option<Uuid> =
        sqlx::query_scalar("SELECT device_id FROM auth_sessions WHERE id=$1")
            .bind(auth.session_id)
            .fetch_one(&mut *tx)
            .await?;
    // Only a signed-in device (with a refresh token) can be signed out, and so stopped.
    let sign_in = sign_in.context(Code::InvalidInput)?;
    sqlx::query("DELETE FROM push_devices WHERE token=$1 OR (user_id=$2 AND device=$3)")
        .bind(&input.token)
        .bind(auth.user_id)
        .bind(&input.device)
        .execute(&mut *tx)
        .await?;
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO push_devices(id,user_id,sign_in,device,platform,token,locale)
         VALUES($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(id)
    .bind(auth.user_id)
    .bind(sign_in)
    .bind(&input.device)
    .bind(&input.platform)
    .bind(&input.token)
    .bind(if input.locale.starts_with("en") {
        "en"
    } else {
        "zh"
    })
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"id": id})))
}

/// `DELETE /devices/{device}`: stop notifying one of the caller's devices.
pub async fn unregister(
    State(app): State<App>,
    headers: HeaderMap,
    Path(device): Path<String>,
) -> ApiResult<Json<Value>> {
    let auth = crate::http::authenticate(&app, &headers).await?;
    let removed = sqlx::query("DELETE FROM push_devices WHERE user_id=$1 AND device=$2")
        .bind(auth.user_id)
        .bind(&device)
        .execute(&app.store.pool)
        .await?
        .rows_affected();
    Ok(Json(json!({"removed": removed > 0})))
}

/// Mark the user's `device` as connected now, or no longer.
pub async fn online(app: &App, user: Uuid, device: &str, online: bool) {
    if !app.config().push.enabled {
        return;
    }
    let result = sqlx::query(
        "UPDATE push_devices SET online_until=CASE WHEN $3 THEN now()+($4::bigint*interval '1 second') END
         WHERE user_id=$1 AND device=$2",
    )
    .bind(user)
    .bind(device)
    .bind(online)
    .bind(ONLINE_SECS)
    .execute(&app.store.pool)
    .await;
    if let Err(error) = result {
        tracing::debug!(%error, "device presence not recorded");
    }
}

/// What a notification is about.
pub enum Notice<'a> {
    /// An approval or a question is waiting.
    Action {
        id: Uuid,
        kind: &'a str,
        summary: &'a str,
    },
    /// The run ended: `completed` or `failed`.
    Ended { run: Uuid, state: &'a str },
}

/// Enqueue a notification about conversation `conv` in `tx`, if its creator has a device to
/// notify (devices register only while notifications are on).
pub async fn notify(
    tx: &mut Tx<'_>,
    org: Uuid,
    conv: Uuid,
    notice: Notice<'_>,
) -> anyhow::Result<()> {
    let user: Option<Uuid> = sqlx::query_scalar(
        "SELECT c.creator_id FROM conversations c WHERE c.org_id=$1 AND c.id=$2 AND NOT c.temporary
         AND EXISTS(SELECT 1 FROM push_devices d WHERE d.user_id=c.creator_id)",
    )
    .bind(org)
    .bind(conv)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(user) = user else {
        return Ok(());
    };
    let (payload, key) = match notice {
        Notice::Action { id, kind, summary } => (
            json!({"user": user, "conversation": conv, "kind": kind, "action": id,
                   "summary": summary.chars().take(200).collect::<String>()}),
            format!("action:{id}"),
        ),
        Notice::Ended { run, state } => (
            json!({"user": user, "conversation": conv, "kind": state, "run": run}),
            format!("run:{run}"),
        ),
    };
    enqueue(tx, org, payload, &key).await?;
    Ok(())
}

/// Enqueue a push job in `tx`; returns its id, or that of the queued job with the same `key`
/// it was merged into (keeping that job's payload, due no later than now).
pub async fn enqueue(
    tx: &mut Tx<'_>,
    org: Uuid,
    payload: Value,
    key: &str,
) -> anyhow::Result<Uuid> {
    Ok(sqlx::query_scalar(
        "INSERT INTO push_jobs(id,org_id,payload,dedupe_key) VALUES($1,$2,$3,$4)
         ON CONFLICT (org_id,dedupe_key) WHERE dedupe_key IS NOT NULL AND state='queued'
         DO UPDATE SET run_at=least(push_jobs.run_at,excluded.run_at)
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(org)
    .bind(payload)
    .bind(key)
    .fetch_one(&mut **tx)
    .await?)
}

/// A claimed push job.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Job {
    pub id: Uuid,
    pub org_id: Uuid,
    pub payload: Value,
    pub claim_token: Uuid,
}

/// Mark `job` completed. False if the claim was lost to another worker.
pub async fn finish(tx: &mut Tx<'_>, job: &Job) -> anyhow::Result<bool> {
    Ok(sqlx::query(
        "UPDATE push_jobs SET state='completed',finished_at=now(),lease_until=NULL,error=NULL
         WHERE org_id=$1 AND id=$2 AND state='running' AND claim_token=$3",
    )
    .bind(job.org_id)
    .bind(job.id)
    .bind(job.claim_token)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        > 0)
}

/// Requeue `job` with backoff, or mark it dead once it has used its attempts.
async fn fail(app: &App, job: &Job, error: &str) -> anyhow::Result<()> {
    let mut tx = app.store.tx(job.org_id).await?;
    sqlx::query(
        "UPDATE push_jobs SET
           state=CASE WHEN attempts>=$5 THEN 'dead' ELSE 'queued' END,
           finished_at=CASE WHEN attempts>=$5 THEN now() END,
           run_at=now()+least(3600,power(2,least(attempts,12))::int)*interval '1 second',
           error=$4,lease_until=NULL,claim_token=NULL
         WHERE org_id=$1 AND id=$2 AND state='running' AND claim_token=$3",
    )
    .bind(job.org_id)
    .bind(job.id)
    .bind(job.claim_token)
    .bind(error)
    .bind(MAX_ATTEMPTS)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Claim the next due job in `org`: queued and due, or running past its lease with attempts
/// left.
async fn claim(app: &App, org: Uuid) -> anyhow::Result<Option<Job>> {
    let mut tx = app.store.tx(org).await?;
    let job = sqlx::query_as(
        "WITH next AS (
           SELECT id FROM push_jobs WHERE org_id=$1
             AND ((state='queued' AND run_at<=now())
                  OR (state='running' AND lease_until<now() AND attempts<$4))
           ORDER BY run_at,id LIMIT 1 FOR UPDATE SKIP LOCKED)
         UPDATE push_jobs j SET state='running',attempts=attempts+1,claim_token=$2,
           lease_until=now()+$3*interval '1 millisecond'
         FROM next WHERE j.org_id=$1 AND j.id=next.id
         RETURNING j.id,j.org_id,j.payload,j.claim_token",
    )
    .bind(org)
    .bind(Uuid::now_v7())
    .bind(LEASE.as_millis() as f64)
    .bind(MAX_ATTEMPTS)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(job)
}

/// Deliver the due jobs; returns how many ran.
pub async fn run_due(app: &App, worker: &PgPool) -> anyhow::Result<usize> {
    let orgs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT org_id FROM push_jobs
         WHERE (state='queued' AND run_at<=now())
            OR (state='running' AND lease_until<now() AND attempts<$1)
         LIMIT 100",
    )
    .bind(MAX_ATTEMPTS)
    .fetch_all(worker)
    .await?;
    let mut ran = 0;
    for org in orgs {
        for _ in 0..PER_ORG {
            let Some(job) = claim(app, org).await? else {
                break;
            };
            ran += 1;
            match tokio::time::timeout(LEASE, deliver(app, &job)).await {
                Ok(Ok(())) => {
                    let mut tx = app.store.tx(job.org_id).await?;
                    finish(&mut tx, &job).await?;
                    tx.commit().await?;
                }
                Ok(Err(error)) => {
                    tracing::warn!(job = %job.id, error = format!("{error:#}"), "push failed");
                    fail(app, &job, crate::error::public_code(&error).as_str()).await?;
                }
                Err(_) => {
                    tracing::warn!(job = %job.id, "push exceeded its lease");
                    fail(app, &job, "LEASE_EXCEEDED").await?;
                }
            }
        }
    }
    Ok(ran)
}

/// Mark jobs dead whose last attempt's lease ran out (a delivery that took its process down
/// would otherwise be retried forever), and delete finished jobs: completed after a week,
/// dead after thirty days.
pub async fn sweep(app: &App, worker: &PgPool) -> anyhow::Result<()> {
    let orgs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT org_id FROM push_jobs WHERE
           (state='running' AND lease_until<now() AND attempts>=$1)
           OR (state='completed' AND finished_at<now()-interval '7 days')
           OR (state='dead' AND finished_at<now()-interval '30 days')
         LIMIT 1000",
    )
    .bind(MAX_ATTEMPTS)
    .fetch_all(worker)
    .await?;
    for org in orgs {
        let mut tx = app.store.tx(org).await?;
        sqlx::query(
            "UPDATE push_jobs SET state='dead',finished_at=now(),lease_until=NULL,claim_token=NULL,
               error='LEASE_EXPIRED'
             WHERE org_id=$1 AND state='running' AND lease_until<now() AND attempts>=$2",
        )
        .bind(org)
        .bind(MAX_ATTEMPTS)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM push_jobs WHERE org_id=$1 AND (
               (state='completed' AND finished_at<now()-interval '7 days')
               OR (state='dead' AND finished_at<now()-interval '30 days'))",
        )
        .bind(org)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

/// The Owner role's push loop.
pub async fn worker(app: App, worker: PgPool) -> anyhow::Result<()> {
    let mut swept: Option<tokio::time::Instant> = None;
    loop {
        if swept.is_none_or(|at| at.elapsed() >= SWEEP_EVERY) {
            sweep(&app, &worker).await?;
            swept = Some(tokio::time::Instant::now());
        }
        if run_due(&app, &worker).await? == 0 {
            tokio::time::sleep(IDLE).await;
        }
    }
}

/// The words of a notification, in the device's language.
fn words(locale: &str, kind: &str, title: &str, summary: &str) -> (String, String) {
    let en = locale == "en";
    match kind {
        "approval" if en => (format!("Approval needed · {title}"), summary.to_owned()),
        "approval" => (format!("需要审批 · {title}"), summary.to_owned()),
        "question" if en => (format!("Question for you · {title}"), summary.to_owned()),
        "question" => (format!("等待你的回答 · {title}"), summary.to_owned()),
        "failed" if en => (title.to_owned(), "The run failed.".to_owned()),
        "failed" => (title.to_owned(), "运行失败。".to_owned()),
        _ if en => (title.to_owned(), "Done.".to_owned()),
        _ => (title.to_owned(), "已完成。".to_owned()),
    }
}

async fn deliver(app: &App, job: &Job) -> anyhow::Result<()> {
    if !app.config().push.enabled {
        return Ok(());
    }
    let user: Uuid = serde_json::from_value(job.payload["user"].clone())?;
    let conv: Uuid = serde_json::from_value(job.payload["conversation"].clone())?;
    let kind = job.payload["kind"].as_str().unwrap_or("completed");
    let summary = job.payload["summary"].as_str().unwrap_or_default();
    let title = {
        let mut tx = app.store.tx(job.org_id).await?;
        let title: Option<String> = sqlx::query_scalar(
            "SELECT title FROM conversations WHERE org_id=$1 AND id=$2 AND state IN ('active','archived')",
        )
        .bind(job.org_id)
        .bind(conv)
        .fetch_optional(&mut *tx)
        .await?;
        // An action answered meanwhile (on another device) is no longer news.
        let waiting = match job.payload["action"].as_str() {
            Some(action) => sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pending_actions WHERE org_id=$1 AND id=$2::uuid AND status='pending')",
            )
            .bind(job.org_id)
            .bind(action)
            .fetch_one(&mut *tx)
            .await?,
            None => true,
        };
        tx.commit().await?;
        match title {
            Some(title) if waiting => title,
            _ => return Ok(()),
        }
    };
    let devices: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT d.id,d.token,d.locale FROM push_devices d JOIN auth_refresh r ON r.id=d.sign_in
         WHERE d.user_id=$1 AND r.revoked_at IS NULL AND r.expires_at>now()
           AND coalesce(d.online_until<now(),true)",
    )
    .bind(user)
    .fetch_all(&app.store.pool)
    .await?;
    if devices.is_empty() {
        return Ok(());
    }
    let fcm = Fcm::load(&app.config().push.fcm_credentials).await?;
    let access = fcm.access_token(&app.http.identity).await?;
    let mut retry = None;
    for (device, token, locale) in devices {
        let (heading, body) = words(&locale, kind, &title, summary);
        let message = json!({"message": {
            "token": token,
            "notification": {"title": heading, "body": body},
            "data": {"org_id": job.org_id.to_string(), "conversation_id": conv.to_string(), "kind": kind},
            "android": {"priority": "high", "notification": {
                "channel_id": "ryzhub_events", "tag": conv.to_string(),
            }},
        }});
        let response = app
            .http
            .identity
            .post(format!(
                "{}/v1/projects/{}/messages:send",
                app.config().push.fcm_endpoint.trim_end_matches('/'),
                fcm.project_id
            ))
            .bearer_auth(&access)
            .json(&message)
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                let status = response.status();
                let error: Value = response.json().await.unwrap_or_default();
                if gone(status.as_u16(), &error) {
                    sqlx::query("DELETE FROM push_devices WHERE id=$1")
                        .bind(device)
                        .execute(&app.store.pool)
                        .await?;
                } else {
                    retry = Some(format!("FCM {status}: {error}"));
                }
            }
            Err(error) => retry = Some(format!("FCM unreachable: {error}")),
        }
    }
    // Retried as a whole; a device notified twice shows one notification (same tag).
    match retry {
        Some(error) => anyhow::bail!(error),
        None => Ok(()),
    }
}

/// FCM says the token is not (or no longer) a device's.
fn gone(status: u16, error: &Value) -> bool {
    let code = error["error"]["details"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|detail| detail["errorCode"].as_str())
        .next()
        .unwrap_or_default();
    status == 404 || code == "UNREGISTERED" || (status == 400 && code == "INVALID_ARGUMENT")
}

/// A Firebase service account.
#[derive(Deserialize)]
struct Fcm {
    project_id: String,
    client_email: String,
    private_key: String,
    #[serde(default = "google_token_uri")]
    token_uri: String,
}

fn google_token_uri() -> String {
    "https://oauth2.googleapis.com/token".into()
}

/// The service account, read once.
static ACCOUNT: tokio::sync::OnceCell<Fcm> = tokio::sync::OnceCell::const_new();
/// The access token last obtained, until shortly before it expires.
static TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);

impl Fcm {
    async fn load(path: &str) -> anyhow::Result<&'static Self> {
        ACCOUNT
            .get_or_try_init(|| async {
                let text =
                    std::fs::read_to_string(path).context("reading the FCM service account")?;
                Ok(serde_json::from_str(&text)?)
            })
            .await
    }

    /// An OAuth access token for FCM, from a JWT the service account signs.
    async fn access_token(&self, http: &reqwest::Client) -> anyhow::Result<String> {
        if let Some((token, until)) = TOKEN.lock().unwrap().as_ref()
            && *until > Instant::now()
        {
            return Ok(token.clone());
        }
        let now = chrono::Utc::now().timestamp();
        let claims = json!({
            "iss": self.client_email, "aud": self.token_uri,
            "scope": "https://www.googleapis.com/auth/firebase.messaging",
            "iat": now, "exp": now + 3600,
        });
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(self.private_key.as_bytes())?;
        let assertion = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            &key,
        )?;
        let reply: Value = http
            .post(&self.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let token = reply["access_token"]
            .as_str()
            .context("no access token")?
            .to_owned();
        let lifetime = reply["expires_in"]
            .as_u64()
            .unwrap_or(3600)
            .saturating_sub(300);
        let until = Instant::now() + Duration::from_secs(lifetime);
        *TOKEN.lock().unwrap() = Some((token.clone(), until));
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_speak_the_device_language_and_know_dead_tokens() {
        assert_eq!(
            words("en", "approval", "Deploy", "Run rm?"),
            ("Approval needed · Deploy".into(), "Run rm?".into())
        );
        assert_eq!(words("zh", "completed", "部署", "").1, "已完成。");
        let unregistered = json!({"error": {"details": [{"errorCode": "UNREGISTERED"}]}});
        assert!(gone(404, &json!({})));
        assert!(gone(
            400,
            &json!({"error": {"details": [{"errorCode": "INVALID_ARGUMENT"}]}})
        ));
        assert!(gone(403, &unregistered));
        assert!(!gone(503, &json!({})));
    }
}
