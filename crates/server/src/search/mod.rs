//! Versioned index publication and live source authorization, shared by REST/Chat/MCP.
pub mod embedding;
use crate::{
    app::App,
    error::{ApiResult, Code},
    store::{Tx, context_in},
};
use anyhow::ensure;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
};
use chrono::{DateTime, Utc};
use embedding::{EmbeddingProvider, OpenAiEmbedding};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub mode: String,
    pub project: Option<Uuid>,
    pub conversation: Option<Uuid>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub run: Option<Uuid>,
}
#[derive(Clone, Copy, Default)]
pub struct Boundary {
    pub agent: bool,
    pub project_only: Option<Uuid>,
}

pub trait SearchBackend {
    #[allow(clippy::too_many_arguments)]
    fn search(
        &self,
        tx: &mut Tx<'_>,
        org: Uuid,
        user: Uuid,
        q: &SearchQuery,
        boundary: Boundary,
        kind: &str,
        vector: Option<String>,
        model: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<Value>>> + Send;
}
pub struct PostgresSearch;
impl SearchBackend for PostgresSearch {
    #[allow(clippy::too_many_arguments)]
    async fn search(
        &self,
        tx: &mut Tx<'_>,
        org: Uuid,
        user: Uuid,
        q: &SearchQuery,
        boundary: Boundary,
        kind: &str,
        vector: Option<String>,
        model: &str,
    ) -> anyhow::Result<Vec<Value>> {
        sqlx::query("SET LOCAL hnsw.iterative_scan = 'strict_order'")
            .execute(&mut **tx)
            .await?;
        let sql = include_str!("retrieve.sql");
        Ok(sqlx::query_scalar(sql)
            .bind(org)
            .bind(user)
            .bind(&q.q)
            .bind(q.project)
            .bind(q.conversation)
            .bind(vector)
            .bind(model)
            .bind(kind)
            .bind(boundary.agent)
            .bind(boundary.project_only)
            .bind(q.from)
            .bind(q.to)
            .bind(if q.mode.is_empty() {
                "fulltext"
            } else {
                &q.mode
            })
            .bind(q.run)
            .fetch_all(&mut **tx)
            .await?)
    }
}

pub async fn retrieve(
    app: &App,
    org: Uuid,
    user: Uuid,
    q: &SearchQuery,
    boundary: Boundary,
    kind: &str,
) -> anyhow::Result<Vec<Value>> {
    ensure!(app.config.search.enabled, Code::TemporarilyUnavailable);
    ensure!(
        !q.q.trim().is_empty()
            && q.q.len() <= 512
            && matches!(q.mode.as_str(), "" | "fulltext" | "vector" | "hybrid"),
        Code::InvalidArguments
    );
    ensure!(
        q.from.zip(q.to).is_none_or(|(from, to)| from <= to),
        Code::InvalidArguments
    );
    let vector = if matches!(q.mode.as_str(), "vector" | "hybrid") {
        let vectors = OpenAiEmbedding
            .embed(app, org, user, vec![q.q.clone()])
            .await;
        match vectors {
            Ok(vectors) => Some(embedding::vector_literal(&vectors[0])),
            Err(error) if q.mode == "hybrid" => {
                tracing::warn!(error=%error, "hybrid search using fulltext while embedding is unavailable");
                None
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let mut tx = app.store.tx(org).await?;
    context_in(&mut tx, user, org).await?;
    let rows = PostgresSearch
        .search(
            &mut tx,
            org,
            user,
            q,
            boundary,
            kind,
            vector,
            &app.config.search.embedding_model,
        )
        .await?;
    tx.commit().await?;
    Ok(rows)
}
pub async fn endpoint(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<Value>> {
    let ctx = crate::http::member(&app, &headers, org).await?;
    let rows = retrieve(&app, org, ctx.user_id, &q, Boundary::default(), "history").await?;
    Ok(Json(json!({"matches":rows})))
}
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct Job {
    pub org_id: Uuid,
    pub source_id: Uuid,
    pub source_kind: String,
    pub source_version: i64,
    pub op: String,
    pub claim_token: Uuid,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Source {
    pub org_id: Uuid,
    pub id: Uuid,
    pub owner_user_id: Uuid,
    pub project_id: Option<Uuid>,
    pub conversation_id: Option<Uuid>,
    pub item_id: Option<Uuid>,
    pub seq: i64,
    pub doc_kind: String,
    pub text: String,
    pub source_version: i64,
    pub deleted: bool,
}
pub async fn claim(app: &App, org: Uuid) -> anyhow::Result<Option<Job>> {
    let mut tx = app.store.tx(org).await?;
    let row=sqlx::query_as("WITH next AS (SELECT source_id,source_version FROM search_jobs WHERE org_id=$1
        AND ((state='queued' AND available_at<=now()) OR (state='running' AND lease_until<now()))
        ORDER BY priority DESC,available_at,created_at LIMIT 1 FOR UPDATE SKIP LOCKED)
        UPDATE search_jobs j SET state='running',attempts=attempts+1,lease_until=now()+interval '5 minutes',claim_token=$2
        FROM next n WHERE j.org_id=$1 AND j.source_id=n.source_id AND j.source_version=n.source_version
        RETURNING j.org_id,j.source_id,j.source_kind,j.source_version,j.op,j.claim_token")
        .bind(org).bind(Uuid::now_v7()).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn source(tx: &mut Tx<'_>, job: &Job) -> anyhow::Result<Option<Source>> {
    Ok(sqlx::query_as("SELECT org_id,id,owner_user_id,project_id,conversation_id,item_id,seq,doc_kind,text,source_version,deleted
        FROM search_sources WHERE org_id=$1 AND id=$2 AND doc_kind=$3")
        .bind(job.org_id).bind(job.source_id).bind(&job.source_kind).fetch_optional(&mut **tx).await?)
}
/// Preparation has no index writes. Publication locks the source, then the job; source writes
/// take the same order through their trigger. Reclaimed workers cannot commit with old tokens.
pub async fn publish(
    app: &App,
    job: &Job,
    prepared: Option<Source>,
    vectors: Vec<Vec<f32>>,
) -> anyhow::Result<bool> {
    publish_with_retry(app, job, prepared, vectors, None).await
}
async fn publish_with_retry(
    app: &App,
    job: &Job,
    prepared: Option<Source>,
    vectors: Vec<Vec<f32>>,
    retry_error: Option<&str>,
) -> anyhow::Result<bool> {
    let mut tx = app.store.tx(job.org_id).await?;
    let lock = match job.source_kind.as_str() {
        "title" => "SELECT id FROM conversations WHERE org_id=$1 AND id=$2 FOR SHARE",
        "message" => "SELECT id FROM items WHERE org_id=$1 AND id=$2 FOR SHARE",
        "project_file" => "SELECT id FROM project_files WHERE org_id=$1 AND id=$2 FOR SHARE",
        "memory" => "SELECT id FROM memory_items WHERE org_id=$1 AND id=$2 FOR SHARE",
        _ => anyhow::bail!(Code::InvalidArguments),
    };
    sqlx::query(lock)
        .bind(job.org_id)
        .bind(job.source_id)
        .fetch_optional(&mut *tx)
        .await?;
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM search_jobs WHERE org_id=$1 AND source_id=$2
        AND source_version=$3 AND state='running' AND claim_token=$4 AND lease_until>now() FOR UPDATE)")
        .bind(job.org_id).bind(job.source_id).bind(job.source_version).bind(job.claim_token).fetch_one(&mut *tx).await?;
    if !valid {
        return Ok(false);
    }
    let live = source(&mut tx, job).await?;
    let current = match (&live, &prepared) {
        (Some(live), Some(old)) => {
            live.source_version == job.source_version && live.text == old.text
        }
        (None, _) => job.op == "delete",
        _ => false,
    };
    if current {
        let fallback = Source {
            org_id: job.org_id,
            id: job.source_id,
            owner_user_id: Uuid::nil(),
            project_id: None,
            conversation_id: None,
            item_id: None,
            seq: 0,
            doc_kind: job.source_kind.clone(),
            text: String::new(),
            source_version: job.source_version,
            deleted: true,
        };
        let s = live.as_ref().unwrap_or(&fallback);
        let deleted = s.deleted || job.op == "delete";
        let changed=sqlx::query("INSERT INTO search_docs(org_id,id,owner_user_id,project_id,conversation_id,item_id,seq,doc_kind,text,source_version,deleted)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) ON CONFLICT(org_id,id) DO UPDATE SET
            owner_user_id=excluded.owner_user_id,project_id=excluded.project_id,conversation_id=excluded.conversation_id,
            item_id=excluded.item_id,seq=excluded.seq,doc_kind=excluded.doc_kind,text=excluded.text,
            source_version=excluded.source_version,deleted=excluded.deleted WHERE search_docs.source_version<=excluded.source_version")
            .bind(s.org_id).bind(s.id).bind(s.owner_user_id).bind(s.project_id).bind(s.conversation_id).bind(s.item_id)
            .bind(s.seq).bind(&s.doc_kind).bind(if deleted{""}else{&s.text}).bind(job.source_version).bind(deleted)
            .execute(&mut *tx).await?.rows_affected()>0;
        if changed {
            sqlx::query("DELETE FROM search_chunks WHERE org_id=$1 AND doc_id=$2")
                .bind(job.org_id)
                .bind(job.source_id)
                .execute(&mut *tx)
                .await?;
            if !deleted && !vectors.is_empty() {
                let chunks = embedding::chunks(&s.text);
                ensure!(vectors.len() == chunks.len(), "embedding count mismatch");
                for (n, (text, vector)) in chunks.iter().zip(vectors.iter()).enumerate() {
                    sqlx::query("INSERT INTO search_chunks(id,org_id,doc_id,chunk_no,text,embedding,embedding_model,source_version)
                        VALUES($1,$2,$3,$4,$5,$6::text::halfvec,$7,$8)")
                        .bind(Uuid::now_v7()).bind(job.org_id).bind(job.source_id).bind(n as i32).bind(text)
                        .bind(embedding::vector_literal(vector)).bind(&app.config.search.embedding_model).bind(job.source_version)
                        .execute(&mut *tx).await?;
                }
            }
        }
    }
    if let Some(error) = retry_error
        && current
    {
        sqlx::query("UPDATE search_jobs SET state=CASE WHEN attempts>=$4 THEN 'dead' ELSE 'queued' END,
            available_at=now()+least(3600,power(2,least(attempts,12))::int)*interval '1 second',error=$5,lease_until=NULL
            WHERE org_id=$1 AND source_id=$2 AND source_version=$3")
            .bind(job.org_id).bind(job.source_id).bind(job.source_version).bind(app.config.search.max_attempts).bind(error).execute(&mut *tx).await?;
    } else {
        sqlx::query("UPDATE search_jobs SET state='completed',lease_until=NULL,error=NULL WHERE org_id=$1 AND source_id=$2 AND source_version=$3")
            .bind(job.org_id).bind(job.source_id).bind(job.source_version).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(current)
}
pub async fn process(app: &App, job: &Job) -> anyhow::Result<bool> {
    let mut tx = app.store.tx(job.org_id).await?;
    let prepared = source(&mut tx, job).await?;
    let memory_allowed = if let Some(s) = &prepared
        && s.doc_kind == "memory"
    {
        let policy =
            crate::store::effective_policy(&mut tx, &app.config.policy, job.org_id).await?;
        let prefs = crate::memory::preferences(&mut tx, job.org_id, s.owner_user_id).await?;
        policy.allow_memory != Some(false) && prefs["enabled"] == true
    } else {
        true
    };
    tx.commit().await?;
    let vectors = if let Some(s) = &prepared
        && !s.deleted
        && s.source_version == job.source_version
        && s.doc_kind != "title"
        && memory_allowed
        && !app.config.search.embedding_url.is_empty()
    {
        let mut vectors = Vec::new();
        for batch in embedding::chunks(&s.text).chunks(64) {
            match OpenAiEmbedding
                .embed(app, job.org_id, s.owner_user_id, batch.to_vec())
                .await
            {
                Ok(batch) => vectors.extend(batch),
                Err(error) => {
                    // Publish the current document without stale vectors; the worker retries
                    // this same version for vectors without blocking fulltext availability.
                    tracing::warn!(source=%job.source_id,error=%error,"embedding deferred; publishing fulltext with atomic retry state");
                    return publish_with_retry(
                        app,
                        job,
                        prepared.clone(),
                        vec![],
                        Some(crate::error::public_code(&error).as_str()),
                    )
                    .await;
                }
            }
        }
        vectors
    } else {
        vec![]
    };
    publish(app, job, prepared, vectors).await
}
pub async fn worker(app: App, worker: PgPool) -> anyhow::Result<()> {
    loop {
        let orgs:Vec<Uuid>=sqlx::query_scalar("SELECT DISTINCT org_id FROM search_jobs WHERE
            (state='queued' AND available_at<=now()) OR (state='running' AND lease_until<now()) LIMIT 100")
            .fetch_all(&worker).await?;
        let mut progressed = false;
        for org in orgs {
            for _ in 0..32 {
                let Some(job) = claim(&app, org).await? else {
                    break;
                };
                progressed = true;
                if let Err(error) = process(&app, &job).await {
                    tracing::warn!(source=%job.source_id,error=%error,"index preparation failed");
                    let mut tx = app.store.tx(org).await?;
                    sqlx::query("UPDATE search_jobs SET state=CASE WHEN attempts>=$5 THEN 'dead' ELSE 'queued' END,
                        available_at=now()+(least(3600,power(2,least(attempts,12))::int)*interval '1 second'),
                        error=$6,lease_until=NULL WHERE org_id=$1 AND source_id=$2 AND source_version=$3 AND claim_token=$4")
                        .bind(org).bind(job.source_id).bind(job.source_version).bind(job.claim_token)
                        .bind(app.config.search.max_attempts).bind(crate::error::public_code(&error).as_str()).execute(&mut *tx).await?;
                    tx.commit().await?;
                }
            }
        }
        // Completed tasks and tombstones have bounded retention. Missing/live sources still
        // reject late tasks after tombstone collection.
        if !progressed {
            let orgs:Vec<Uuid>=sqlx::query_scalar("SELECT DISTINCT org_id FROM search_jobs WHERE state='completed' AND created_at<now()-interval '7 days' LIMIT 100").fetch_all(&worker).await?;
            for org in orgs {
                let mut tx = app.store.tx(org).await?;
                sqlx::query("DELETE FROM search_jobs WHERE org_id=$1 AND state='completed' AND created_at<now()-interval '7 days'").bind(org).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM search_docs WHERE org_id=$1 AND deleted AND created_at<now()-interval '7 days'").bind(org).execute(&mut *tx).await?;
                tx.commit().await?;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
}
