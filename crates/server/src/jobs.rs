//! Durable background jobs.
//!
//! A job is enqueued with [`enqueue`] inside the transaction whose commit should cause it, so
//! it exists exactly when that commit does. External side effects (notifications, webhooks)
//! are jobs too. The Owner role's worker claims due jobs with `FOR UPDATE SKIP LOCKED`, runs
//! the [`Handler`] registered for the kind, and retries failures with exponential backoff
//! until `max_attempts`, after which the job is `dead`.
//!
//! Delivery is at least once: a handler that outlives its lease may run again elsewhere, so
//! handlers must be idempotent. Completion is fenced by the claim token, so a worker whose
//! lease was taken over cannot complete or fail the job. Kinds this binary has no handler for
//! are left queued for a version that does.
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{app::App, store::Tx};

/// Idle wait between scans for due jobs.
const IDLE: Duration = Duration::from_secs(1);
/// How often finished jobs are swept.
const SWEEP_EVERY: Duration = Duration::from_secs(600);
/// Jobs claimed per organization per scan, so one busy organization cannot starve others.
const PER_ORG: usize = 32;

/// A job to enqueue. Deduplicated jobs should carry identifiers rather than state, since a
/// later enqueue with the same key keeps the first payload.
pub struct NewJob<'a> {
    pub kind: &'a str,
    pub payload: Value,
    /// While a queued job of this kind has this key, enqueueing returns that job (moved
    /// earlier if this one is due sooner). A running job does not absorb new enqueues.
    pub dedupe_key: Option<&'a str>,
    pub run_at: Option<DateTime<Utc>>,
    pub priority: i16,
    pub max_attempts: i32,
}

impl<'a> NewJob<'a> {
    pub fn new(kind: &'a str, payload: Value) -> Self {
        Self {
            kind,
            payload,
            dedupe_key: None,
            run_at: None,
            priority: 0,
            max_attempts: 8,
        }
    }
}

/// A claimed job.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Job {
    pub id: Uuid,
    pub org_id: Uuid,
    pub kind: String,
    pub payload: Value,
    /// Including this one.
    pub attempts: i32,
    pub claim_token: Uuid,
}

pub type Run = for<'a> fn(&'a App, &'a Job) -> BoxFuture<'a, anyhow::Result<()>>;

/// How a kind of job runs. The handler is cut off when its lease ends.
#[derive(Clone)]
pub struct Handler {
    pub kind: &'static str,
    pub lease: Duration,
    pub run: Run,
}

/// The kinds this binary runs.
pub fn handlers() -> Vec<Handler> {
    vec![crate::push::handler()]
}

/// Enqueue a job in `tx`; returns its id, or the id of the queued job it was merged into.
pub async fn enqueue(tx: &mut Tx<'_>, org: Uuid, job: NewJob<'_>) -> anyhow::Result<Uuid> {
    Ok(sqlx::query_scalar(
        "INSERT INTO jobs(id,org_id,kind,payload,dedupe_key,run_at,priority,max_attempts)
         VALUES($1,$2,$3,$4,$5,coalesce($6,now()),$7,$8)
         ON CONFLICT (org_id,kind,dedupe_key) WHERE dedupe_key IS NOT NULL AND state='queued'
         DO UPDATE SET run_at=least(jobs.run_at,excluded.run_at),
                       priority=greatest(jobs.priority,excluded.priority)
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(org)
    .bind(job.kind)
    .bind(job.payload)
    .bind(job.dedupe_key)
    .bind(job.run_at)
    .bind(job.priority)
    .bind(job.max_attempts)
    .fetch_one(&mut **tx)
    .await?)
}

/// Mark `job` completed in `tx`, so a handler can commit its effects and the completion
/// together. False if the claim was lost to another worker.
pub async fn finish(tx: &mut Tx<'_>, job: &Job) -> anyhow::Result<bool> {
    Ok(sqlx::query(
        "UPDATE jobs SET state='completed',finished_at=now(),lease_until=NULL,error=NULL
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
        "UPDATE jobs SET
           state=CASE WHEN attempts>=max_attempts THEN 'dead' ELSE 'queued' END,
           finished_at=CASE WHEN attempts>=max_attempts THEN now() END,
           run_at=now()+least(3600,power(2,least(attempts,12))::int)*interval '1 second',
           error=$4,lease_until=NULL,claim_token=NULL
         WHERE org_id=$1 AND id=$2 AND state='running' AND claim_token=$3",
    )
    .bind(job.org_id)
    .bind(job.id)
    .bind(job.claim_token)
    .bind(error)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Claim the next due job of `kind` in `org`: queued and due, or running past its lease
/// with attempts left.
async fn claim(app: &App, org: Uuid, kind: &str, lease: Duration) -> anyhow::Result<Option<Job>> {
    let mut tx = app.store.tx(org).await?;
    let job = sqlx::query_as(
        "WITH next AS (
           SELECT id FROM jobs WHERE org_id=$1 AND kind=$2
             AND ((state='queued' AND run_at<=now())
                  OR (state='running' AND lease_until<now() AND attempts<max_attempts))
           ORDER BY priority DESC,run_at,id LIMIT 1 FOR UPDATE SKIP LOCKED)
         UPDATE jobs j SET state='running',attempts=attempts+1,claim_token=$3,
           lease_until=now()+$4*interval '1 millisecond'
         FROM next WHERE j.org_id=$1 AND j.id=next.id
         RETURNING j.id,j.org_id,j.kind,j.payload,j.attempts,j.claim_token",
    )
    .bind(org)
    .bind(kind)
    .bind(Uuid::now_v7())
    .bind(lease.as_millis() as f64)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(job)
}

async fn execute(app: &App, handler: &Handler, job: &Job) -> anyhow::Result<()> {
    let outcome = tokio::time::timeout(handler.lease, (handler.run)(app, job)).await;
    match outcome {
        Ok(Ok(())) => {
            let mut tx = app.store.tx(job.org_id).await?;
            finish(&mut tx, job).await?;
            tx.commit().await?;
        }
        Ok(Err(error)) => {
            tracing::warn!(job = %job.id, kind = %job.kind, error = format!("{error:#}"), "job failed");
            fail(app, job, crate::error::public_code(&error).as_str()).await?;
        }
        Err(_) => {
            tracing::warn!(job = %job.id, kind = %job.kind, "job exceeded its lease");
            fail(app, job, "LEASE_EXCEEDED").await?;
        }
    }
    Ok(())
}

/// Run the due jobs `handlers` cover; returns how many ran.
pub async fn run_due(app: &App, worker: &PgPool, handlers: &[Handler]) -> anyhow::Result<usize> {
    if handlers.is_empty() {
        return Ok(0);
    }
    let kinds: Vec<&str> = handlers.iter().map(|h| h.kind).collect();
    let orgs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT org_id FROM jobs WHERE kind=ANY($1)
           AND ((state='queued' AND run_at<=now())
                OR (state='running' AND lease_until<now() AND attempts<max_attempts))
         LIMIT 100",
    )
    .bind(&kinds)
    .fetch_all(worker)
    .await?;
    let mut ran = 0;
    for org in orgs {
        let mut claimed = 0;
        for handler in handlers {
            while claimed < PER_ORG {
                let Some(job) = claim(app, org, handler.kind, handler.lease).await? else {
                    break;
                };
                claimed += 1;
                execute(app, handler, &job).await?;
            }
        }
        ran += claimed;
    }
    Ok(ran)
}

/// Mark jobs dead whose last attempt's lease ran out (a handler that crashes its process
/// would otherwise be retried forever), and delete finished jobs: completed after a week,
/// dead after thirty days.
pub async fn sweep(app: &App, worker: &PgPool) -> anyhow::Result<()> {
    let orgs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT org_id FROM jobs WHERE
           (state='running' AND lease_until<now() AND attempts>=max_attempts)
           OR (state='completed' AND finished_at<now()-interval '7 days')
           OR (state='dead' AND finished_at<now()-interval '30 days')
         LIMIT 1000",
    )
    .fetch_all(worker)
    .await?;
    for org in orgs {
        let mut tx = app.store.tx(org).await?;
        sqlx::query(
            "UPDATE jobs SET state='dead',finished_at=now(),lease_until=NULL,claim_token=NULL,
               error='LEASE_EXPIRED'
             WHERE org_id=$1 AND state='running' AND lease_until<now() AND attempts>=max_attempts",
        )
        .bind(org)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM jobs WHERE org_id=$1 AND (
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

/// The Owner role's job loop.
pub async fn worker(app: App, worker: PgPool, handlers: Vec<Handler>) -> anyhow::Result<()> {
    let mut swept: Option<tokio::time::Instant> = None;
    loop {
        if swept.is_none_or(|at| at.elapsed() >= SWEEP_EVERY) {
            sweep(&app, &worker).await?;
            swept = Some(tokio::time::Instant::now());
        }
        if run_due(&app, &worker, &handlers).await? == 0 {
            tokio::time::sleep(IDLE).await;
        }
    }
}
