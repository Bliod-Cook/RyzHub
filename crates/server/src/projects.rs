//! Private projects: creator-scoped access, immutable memory boundary and recoverable deletion.
//! Runs reference an immutable, content-addressed revision of the project's instructions and
//! files, so edits affect only later runs and identical content is stored once.
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use ryzhub_execution::{GuestCommand, HostCommand};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::App,
    error::{ApiResult, Code},
    http::member_tx,
    run_config::RunConfig,
    store::{Lease, Tx, conversation_in, effective_policy},
};

mod sources;
pub use sources::{source as file_source, upload as upload_file};

/// A deleted project can be restored this long.
const RECOVERABLE_DAYS: i64 = 30;
/// Total size of a project's knowledge files.
const MAX_PROJECT_BYTES: u64 = 2 * 1024 * 1024;

/// The caller's own project, locked. `active` also rejects a project pending deletion.
async fn owned(
    tx: &mut Tx<'_>,
    org: Uuid,
    user: Uuid,
    id: Uuid,
    active: bool,
) -> anyhow::Result<Value> {
    let p: Value = sqlx::query_scalar(
        "SELECT to_jsonb(p) FROM projects p WHERE org_id=$1 AND creator_id=$2 AND id=$3 AND state<>'deleted' FOR UPDATE",
    )
    .bind(org)
    .bind(user)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .context(Code::Forbidden)?;
    ensure!(!active || p["state"] == "active", Code::ProjectDeleted);
    Ok(p)
}

/// The organization's policy, which must allow projects.
async fn allowed(app: &App, tx: &mut Tx<'_>, org: Uuid) -> anyhow::Result<ryzhub_domain::Policy> {
    let policy = effective_policy(tx, &app.config.policy, org).await?;
    ensure!(policy.allow_projects != Some(false), Code::PolicyDenied);
    Ok(policy)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub name: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default = "empty")]
    pub defaults: Value,
    #[serde(default = "default_memory")]
    pub memory_mode: String,
}

fn empty() -> Value {
    json!({})
}

fn default_memory() -> String {
    "default".into()
}

fn validate(input: &Input) -> anyhow::Result<()> {
    ensure!(
        !input.name.trim().is_empty()
            && input.name.len() <= 256
            && input.instructions.len() <= 65536
            && matches!(input.memory_mode.as_str(), "default" | "project_only"),
        Code::InvalidProject
    );
    ensure!(input.defaults.is_object(), Code::InvalidDefaults);
    Ok(())
}

/// `GET /orgs/{org}/projects`: the caller's projects, including those pending deletion.
pub async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(p) FROM projects p WHERE org_id=$1 AND creator_id=$2 AND state<>'deleted'
         ORDER BY created_at DESC",
    )
    .bind(org)
    .bind(ctx.user_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!(rows)))
}

/// `POST /orgs/{org}/projects`.
pub async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(input): Json<Input>,
) -> ApiResult<Json<Value>> {
    validate(&input)?;
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    allowed(&app, &mut tx, org).await?;
    validate_defaults(&input.defaults, &app, &mut tx, org).await?;
    let row: Value = sqlx::query_scalar(
        "INSERT INTO projects(id,org_id,creator_id,name,instructions,defaults,memory_mode)
         VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING to_jsonb(projects)",
    )
    .bind(Uuid::now_v7())
    .bind(org)
    .bind(ctx.user_id)
    .bind(input.name)
    .bind(input.instructions)
    .bind(input.defaults)
    .bind(input.memory_mode)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(row))
}

/// Default run settings for the project's new conversations must be valid under policy.
async fn validate_defaults(
    defaults: &Value,
    app: &App,
    tx: &mut Tx<'_>,
    org: Uuid,
) -> anyhow::Result<()> {
    let mut v = defaults.clone();
    let mode = v
        .as_object_mut()
        .context(Code::InvalidDefaults)?
        .remove("mode")
        .unwrap_or(json!("chat"));
    let c = RunConfig::resolve(v, mode.as_str().context(Code::InvalidMode)?, &app.config)?;
    c.validate(tx, org, &app.config).await?;
    Ok(())
}

/// `PATCH /orgs/{org}/projects/{project}`. The memory boundary cannot change.
pub async fn update(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
    Json(input): Json<Input>,
) -> ApiResult<Json<Value>> {
    validate(&input)?;
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    allowed(&app, &mut tx, org).await?;
    let old = owned(&mut tx, org, ctx.user_id, id, true).await?;
    require!(
        old["memory_mode"] == input.memory_mode,
        Code::MemoryModeImmutable
    );
    validate_defaults(&input.defaults, &app, &mut tx, org).await?;
    sqlx::query(
        "UPDATE projects SET name=$3,instructions=$4,defaults=$5 WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .bind(input.name)
    .bind(input.instructions)
    .bind(input.defaults)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"updated":true,"effective":"next_turn"})))
}

/// `GET /orgs/{org}/projects/{project}/files`.
pub async fn files(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    owned(&mut tx, org, ctx.user_id, id, true).await?;
    let files: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(f) FROM project_files f WHERE org_id=$1 AND project_id=$2 ORDER BY name",
    )
    .bind(org)
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!(files)))
}

#[derive(Deserialize)]
pub struct FileInput {
    name: String,
    content: String,
}

/// A knowledge file's name: a plain file name.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
}

/// `POST /orgs/{org}/projects/{project}/files`: create or replace a knowledge file, within the
/// policy's size limit for the whole project.
pub async fn put_file(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
    Json(input): Json<FileInput>,
) -> ApiResult<Json<Value>> {
    require!(
        valid_name(&input.name) && input.content.len() <= 1024 * 1024,
        Code::InvalidProjectFile
    );
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let policy = allowed(&app, &mut tx, org).await?;
    owned(&mut tx, org, ctx.user_id, id, true).await?;
    let bytes: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(octet_length(content)),0)::bigint FROM project_files
         WHERE org_id=$1 AND project_id=$2 AND name<>$3",
    )
    .bind(org)
    .bind(id)
    .bind(&input.name)
    .fetch_one(&mut *tx)
    .await?;
    let limit = policy
        .project_file_bytes
        .unwrap_or(MAX_PROJECT_BYTES)
        .min(MAX_PROJECT_BYTES);
    require!(
        bytes as u64 + input.content.len() as u64 <= limit,
        Code::ProjectFileLimit
    );
    sqlx::query(
        "INSERT INTO project_files(id,org_id,project_id,name,content) VALUES($1,$2,$3,$4,$5)
         ON CONFLICT(project_id,name) DO UPDATE SET content=EXCLUDED.content",
    )
    .bind(Uuid::now_v7())
    .bind(org)
    .bind(id)
    .bind(input.name)
    .bind(input.content)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"saved":true,"effective":"next_turn"})))
}

/// `DELETE /orgs/{org}/projects/{project}/files/{file}`.
pub async fn remove_file(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id, file)): Path<(Uuid, Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    owned(&mut tx, org, ctx.user_id, id, true).await?;
    let source: Option<String> = sqlx::query_scalar(
        "DELETE FROM project_files WHERE org_id=$1 AND project_id=$2 AND id=$3 RETURNING source_sha256",
    )
    .bind(org)
    .bind(id)
    .bind(file)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    tx.commit().await?;
    if let Some(source) = source {
        sources::release(&app, org, &source).await;
    }
    Ok(Json(json!({"deleted":true})))
}

#[derive(Deserialize)]
pub struct Move {
    pub project_id: Option<Uuid>,
}

/// `POST /orgs/{org}/conversations/{conv}/project`: move an idle conversation into (or out
/// of) one of the caller's projects.
pub async fn move_conversation(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Json(input): Json<Move>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    conversation_in(&mut tx, &ctx, conv).await?;
    allowed(&app, &mut tx, org).await?;
    if let Some(project) = input.project_id {
        owned(&mut tx, org, ctx.user_id, project, true).await?;
    }
    crate::conversation_commands::idle(&mut tx, org, conv).await?;
    // Deletion may have committed while we waited for the conversation channel.
    conversation_in(&mut tx, &ctx, conv).await?;
    let old: Option<String> = sqlx::query_scalar(
        "SELECT p.memory_mode FROM projects p JOIN conversations c ON c.org_id=p.org_id AND c.project_id=p.id
         WHERE c.org_id=$1 AND c.id=$2",
    )
    .bind(org)
    .bind(conv)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query("UPDATE conversations SET project_id=$3 WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(conv)
        .bind(input.project_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let memory_notice =
        (old.as_deref() == Some("project_only")).then_some("已有记忆保留在原项目内");
    Ok(Json(json!({"moved":true,"memory_notice":memory_notice})))
}

/// `DELETE /orgs/{org}/projects/{project}`: hide the project and its conversations. It can be
/// restored for [`RECOVERABLE_DAYS`]; after that the collector purges it.
pub async fn delete(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    owned(&mut tx, org, ctx.user_id, id, true).await?;
    let convs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM conversations WHERE org_id=$1 AND project_id=$2 ORDER BY id",
    )
    .bind(org)
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    for conv in convs {
        crate::conversation_commands::deletable(&mut tx, org, conv).await?;
    }
    sqlx::query(
        "UPDATE conversations SET project_deleted_state=state,state='deletion_pending'
         WHERE org_id=$1 AND project_id=$2 AND state IN ('active','archived')",
    )
    .bind(org)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE projects SET state='deletion_pending',deleted_at=now() WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({"state":"deletion_pending","recoverable_days":RECOVERABLE_DAYS}),
    ))
}

/// `POST /orgs/{org}/projects/{project}/restore`: undo a deletion within the recovery window.
pub async fn restore(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    owned(&mut tx, org, ctx.user_id, id, false).await?;
    let restored = sqlx::query(
        "UPDATE projects SET state='active',deleted_at=NULL WHERE org_id=$1 AND id=$2
         AND state='deletion_pending' AND deleted_at>now()-($3::bigint*interval '1 day')",
    )
    .bind(org)
    .bind(id)
    .bind(RECOVERABLE_DAYS)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    require!(restored == 1, Code::RestoreExpired);
    sqlx::query(
        "UPDATE conversations SET state=project_deleted_state,project_deleted_state=NULL
         WHERE org_id=$1 AND project_id=$2 AND project_deleted_state IS NOT NULL",
    )
    .bind(org)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"restored":true})))
}

/// Snapshot the project's current instructions and files for a new run. Identical content
/// reuses the existing revision. Returns None for a project that is no longer active.
pub async fn snapshot(tx: &mut Tx<'_>, org: Uuid, project: Uuid) -> anyhow::Result<Option<Uuid>> {
    let query = "WITH content AS (
            SELECT p.id AS project_id,
                   E'\\n\\n<project>Memory boundary: ' || p.memory_mode || E'>\\n' || p.instructions
                   || E'\\nKnowledge files are available read-only in /project and via project_file_search.\\n</project>' AS context,
                   coalesce((SELECT jsonb_object_agg(f.name,f.content) FROM project_files f
                             WHERE f.org_id=p.org_id AND f.project_id=p.id),'{}') AS files
            FROM projects p WHERE p.org_id=$1 AND p.id=$2 AND p.state='active'),
        digested AS (SELECT *, sha256(convert_to(context || files::text,'UTF8')) AS digest FROM content),
        inserted AS (
            INSERT INTO project_revisions(id,org_id,project_id,digest,context,files)
            SELECT $3,$1,project_id,digest,context,files FROM digested
            ON CONFLICT(project_id,digest) DO NOTHING RETURNING id)
        SELECT id FROM inserted
        UNION ALL SELECT r.id FROM project_revisions r JOIN digested d ON r.project_id=d.project_id AND r.digest=d.digest
        LIMIT 1";
    let revision: Option<Uuid> = sqlx::query_scalar(query)
        .bind(org)
        .bind(project)
        .bind(Uuid::now_v7())
        .fetch_optional(&mut **tx)
        .await?;
    if revision.is_some() {
        return Ok(revision);
    }
    // A concurrent run inserted the same revision after this statement's snapshot.
    Ok(sqlx::query_scalar(query)
        .bind(org)
        .bind(project)
        .bind(Uuid::now_v7())
        .fetch_optional(&mut **tx)
        .await?)
}

/// The project context block frozen with the run.
pub async fn instructions(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    config: &crate::config::Config,
) -> anyhow::Result<String> {
    let org = lease.org_id;
    let project: String = sqlx::query_scalar(
        "SELECT coalesce(p.context,'') FROM runs r LEFT JOIN project_revisions p ON p.org_id=r.org_id AND p.id=r.project_revision_id WHERE r.org_id=$1 AND r.id=$2")
        .bind(org).bind(run).fetch_one(&mut **tx).await?;
    let (conversation,user): (Uuid,Uuid) = sqlx::query_as("SELECT c.id,c.creator_id FROM runs r JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id WHERE r.org_id=$1 AND r.id=$2")
        .bind(org).bind(run).fetch_one(&mut **tx).await?;
    let memory = crate::memory::context(tx, config, org, conversation, user).await?;
    crate::memory::announce(tx, lease, run, &memory.used).await?;
    Ok(project + &memory.text)
}

/// What a Work harness is told: how to hand files over, then the run's project context.
pub async fn harness_instructions(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    config: &crate::config::Config,
) -> anyhow::Result<String> {
    let project = instructions(tx, lease, run, config).await?;
    let files = crate::attachments::outputs::INSTRUCTIONS;
    Ok(if project.is_empty() {
        files.to_owned()
    } else {
        format!("{files}\n\n{project}")
    })
}

/// The project files frozen with the run, by name.
async fn run_files(tx: &mut Tx<'_>, org: Uuid, run: Uuid) -> anyhow::Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT f.key,f.value FROM runs r
         JOIN project_revisions p ON p.org_id=r.org_id AND p.id=r.project_revision_id
         CROSS JOIN LATERAL jsonb_each_text(p.files) f
         WHERE r.org_id=$1 AND r.id=$2 ORDER BY f.key",
    )
    .bind(org)
    .bind(run)
    .fetch_all(&mut **tx)
    .await?)
}

/// Search the live indexed project files, restricted to content frozen into this run.
pub async fn search(tx: &mut Tx<'_>, org: Uuid, run: Uuid, query: &str) -> anyhow::Result<Value> {
    ensure!(
        !query.trim().is_empty() && query.len() <= 512,
        Code::InvalidToolArguments
    );
    let (project, user): (Option<Uuid>, Uuid) = sqlx::query_as(
        "SELECT r.project_id,c.creator_id FROM runs r
         JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id
         WHERE r.org_id=$1 AND r.id=$2",
    )
    .bind(org)
    .bind(run)
    .fetch_one(&mut **tx)
    .await?;
    let Some(project) = project else {
        return Ok(json!({"matches":[]}));
    };
    let q = crate::search::SearchQuery {
        q: query.into(),
        project: Some(project),
        run: Some(run),
        ..Default::default()
    };
    let boundary = crate::search::Boundary::default();
    let mut matches =
        crate::search::search(tx, org, user, &q, boundary, "project_file", None, "").await?;
    // A run keeps its immutable knowledge after a file edit. If that version is no
    // longer in the live index (or indexing is pending), query the frozen text with
    // PGroonga, still joining the live project/file/creator/member authorization.
    let indexed: Vec<String> = matches
        .iter()
        .filter_map(|v| v["file"].as_str().map(str::to_owned))
        .collect();
    let mut frozen: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('file',entry.key,'line',1,'text',left(entry.value,1500),
           'snippet_html',array_to_string(pgroonga_snippet_html(entry.value,
             pgroonga_query_extract_keywords(pgroonga_query_escape($4))),' … '))
         FROM runs r JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id
         JOIN projects p ON p.org_id=r.org_id AND p.id=r.project_id
         JOIN project_revisions rev ON rev.org_id=r.org_id AND rev.id=r.project_revision_id
         CROSS JOIN LATERAL jsonb_each_text(rev.files) entry
         JOIN project_files f ON f.org_id=p.org_id AND f.project_id=p.id AND f.name=entry.key
         WHERE r.org_id=$1 AND r.id=$2 AND c.creator_id=$3 AND p.creator_id=$3 AND p.state='active'
           AND c.state IN ('active','archived')
           AND EXISTS(SELECT 1 FROM memberships WHERE org_id=$1 AND user_id=$3)
           AND NOT (entry.key=ANY($5)) AND entry.value &@~ pgroonga_query_escape($4) LIMIT 20",
    )
    .bind(org)
    .bind(run)
    .bind(user)
    .bind(query)
    .bind(&indexed)
    .fetch_all(&mut **tx)
    .await?;
    crate::search::snippets(&mut frozen);
    matches.extend(frozen);
    matches.truncate(20);
    Ok(json!({"matches":matches}))
}

/// Mount the run's project files read-only at `/project` and install its instructions.
pub async fn mount(app: &App, lease: &Lease, run: Uuid, sandbox: Uuid) -> anyhow::Result<()> {
    let (files, instructions) = {
        let mut tx = app.store.fenced(lease).await?;
        let files = run_files(&mut tx, lease.org_id, run).await?;
        let instructions = harness_instructions(&mut tx, lease, run, &app.config).await?;
        tx.commit().await?;
        (files, instructions)
    };
    crate::sandbox::host(
        app,
        lease.org_id,
        sandbox,
        HostCommand::Project {
            sandbox_id: sandbox,
            exec_epoch: lease.epoch as u64,
            files: files.into_iter().collect(),
        },
    )
    .await?;
    crate::sandbox::guest(
        app,
        lease,
        sandbox,
        GuestCommand::ProjectInstructions { text: instructions },
    )
    .await?;
    Ok(())
}

/// Purge a project whose recovery window has passed: destroy its conversations' sandboxes and
/// stored content. Ids and accounting stay as tombstones. Holds the same project and channel
/// locks as restore and command admission.
pub async fn collect(app: &App, org: Uuid, project: Uuid) -> anyhow::Result<bool> {
    let mut tx = app.store.tx(org).await?;
    let expired: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM projects WHERE org_id=$1 AND id=$2 AND state='deletion_pending'
         AND deleted_at<=now()-($3::bigint*interval '1 day') FOR UPDATE",
    )
    .bind(org)
    .bind(project)
    .bind(RECOVERABLE_DAYS)
    .fetch_optional(&mut *tx)
    .await?;
    if expired.is_none() {
        return Ok(false);
    }
    let conversations: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM conversations WHERE org_id=$1 AND project_id=$2 ORDER BY id",
    )
    .bind(org)
    .bind(project)
    .fetch_all(&mut *tx)
    .await?;
    for conv in conversations {
        crate::conversation_commands::purge(app, &mut tx, org, conv).await?;
    }
    let memories: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM memory_items WHERE org_id=$1 AND project_id=$2")
            .bind(org)
            .bind(project)
            .fetch_all(&mut *tx)
            .await?;
    crate::memory::purge_memories(&mut tx, org, &memories).await?;
    let documents: Vec<String> = sqlx::query_scalar(
        "DELETE FROM project_files WHERE org_id=$1 AND project_id=$2 AND source_sha256 IS NOT NULL
         RETURNING source_sha256",
    )
    .bind(org)
    .bind(project)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM project_files WHERE org_id=$1 AND project_id=$2")
        .bind(org)
        .bind(project)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM search_docs WHERE org_id=$1 AND project_id=$2")
        .bind(org)
        .bind(project)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE runs SET project_revision_id=NULL WHERE org_id=$1 AND project_id=$2")
        .bind(org)
        .bind(project)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM project_revisions WHERE org_id=$1 AND project_id=$2")
        .bind(org)
        .bind(project)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE projects SET state='deleted',name='',instructions='',defaults='{}' WHERE org_id=$1 AND id=$2",
    )
        .bind(org)
        .bind(project)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    for document in documents {
        sources::release(app, org, &document).await;
    }
    crate::workspace::collect_org(app, org).await?;
    Ok(true)
}

/// Owner-role loop purging projects whose recovery window has passed.
pub async fn collector(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    loop {
        let projects: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT org_id,id FROM projects WHERE state='deletion_pending'
             AND deleted_at<=now()-($1::bigint*interval '1 day') ORDER BY deleted_at LIMIT 100",
        )
        .bind(RECOVERABLE_DAYS)
        .fetch_all(&worker)
        .await?;
        for (org, project) in projects {
            if let Err(error) = collect(&app, org, project).await {
                tracing::warn!(%org, %project, %error, "project collection deferred");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}
