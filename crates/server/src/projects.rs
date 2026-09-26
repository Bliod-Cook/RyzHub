//! Private projects: creator-scoped access, immutable memory boundary and recoverable deletion.
use crate::{
    api::{ApiResult, App, authenticate},
    run_config::RunConfig,
    store::{Tx, context_in, conversation_in},
};
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

async fn owned(
    tx: &mut Tx<'_>,
    org: Uuid,
    user: Uuid,
    id: Uuid,
    active: bool,
) -> anyhow::Result<Value> {
    let p:Value=sqlx::query_scalar("SELECT to_jsonb(p) FROM projects p WHERE org_id=$1 AND creator_id=$2 AND id=$3 AND state<>'deleted' FOR UPDATE").bind(org).bind(user).bind(id).fetch_optional(&mut **tx).await?.context("FORBIDDEN")?;
    require(!active || p["state"] == "active", "PROJECT_DELETED")?;
    Ok(p)
}
async fn allowed(app: &App, tx: &mut Tx<'_>, org: Uuid) -> anyhow::Result<ryzhub_domain::Policy> {
    let p = crate::llm::gateway::effective_policy(app, tx, org).await?;
    require(p.allow_projects != Some(false), "POLICY_DENIED")?;
    Ok(p)
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
    require(
        !input.name.trim().is_empty()
            && input.name.len() <= 256
            && input.instructions.len() <= 65536
            && matches!(input.memory_mode.as_str(), "default" | "project_only"),
        "INVALID_PROJECT",
    )?;
    require(input.defaults.is_object(), "INVALID_DEFAULTS")?;
    Ok(())
}
pub async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.tx(org).await?;
    context_in(&mut tx, auth.user_id, org).await?;
    let rows:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(p) FROM projects p WHERE org_id=$1 AND creator_id=$2 AND state<>'deleted' ORDER BY created_at DESC").bind(org).bind(auth.user_id).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!(rows)))
}
pub async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(input): Json<Input>,
) -> ApiResult<Json<Value>> {
    validate(&input)?;
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.tx(org).await?;
    context_in(&mut tx, auth.user_id, org).await?;
    allowed(&app, &mut tx, org).await?;
    validate_defaults(&input.defaults, &app, &mut tx, org).await?;
    let id = Uuid::now_v7();
    let row:Value=sqlx::query_scalar("INSERT INTO projects(id,org_id,creator_id,name,instructions,defaults,memory_mode) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING to_jsonb(projects)").bind(id).bind(org).bind(auth.user_id).bind(input.name).bind(input.instructions).bind(input.defaults).bind(input.memory_mode).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(row))
}
async fn validate_defaults(
    defaults: &Value,
    app: &App,
    tx: &mut Tx<'_>,
    org: Uuid,
) -> anyhow::Result<()> {
    let mut v = defaults.clone();
    let mode = v
        .as_object_mut()
        .context("INVALID_DEFAULTS")?
        .remove("mode")
        .unwrap_or(json!("chat"));
    let c = RunConfig::resolve(v, mode.as_str().context("INVALID_MODE")?, &app.config)?;
    c.validate(tx, org, &app.config).await
}
pub async fn update(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
    Json(input): Json<Input>,
) -> ApiResult<Json<Value>> {
    validate(&input)?;
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.tx(org).await?;
    context_in(&mut tx, auth.user_id, org).await?;
    allowed(&app, &mut tx, org).await?;
    let old = owned(&mut tx, org, auth.user_id, id, true).await?;
    require(
        old["memory_mode"] == input.memory_mode,
        "MEMORY_MODE_IMMUTABLE",
    )?;
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
pub async fn files(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.tx(org).await?;
    context_in(&mut tx, auth.user_id, org).await?;
    owned(&mut tx, org, auth.user_id, id, true).await?;
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
pub async fn put_file(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
    Json(input): Json<FileInput>,
) -> ApiResult<Json<Value>> {
    require(
        !input.name.is_empty()
            && input.name.len() <= 128
            && !input.name.starts_with('.')
            && !input.name.contains(['/', '\\', '\0'])
            && input.content.len() <= 1024 * 1024,
        "INVALID_PROJECT_FILE",
    )?;
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.tx(org).await?;
    context_in(&mut tx, auth.user_id, org).await?;
    let policy = allowed(&app, &mut tx, org).await?;
    owned(&mut tx, org, auth.user_id, id, true).await?;
    let bytes:i64=sqlx::query_scalar("SELECT coalesce(sum(octet_length(content)),0)::bigint FROM project_files WHERE org_id=$1 AND project_id=$2 AND name<>$3").bind(org).bind(id).bind(&input.name).fetch_one(&mut *tx).await?;
    require(
        bytes as u64 + input.content.len() as u64
            <= policy
                .project_file_bytes
                .unwrap_or(2 * 1024 * 1024)
                .min(2 * 1024 * 1024),
        "PROJECT_FILE_LIMIT",
    )?;
    sqlx::query("INSERT INTO project_files(id,org_id,project_id,name,content) VALUES($1,$2,$3,$4,$5) ON CONFLICT(project_id,name) DO UPDATE SET content=EXCLUDED.content").bind(Uuid::now_v7()).bind(org).bind(id).bind(input.name).bind(input.content).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"saved":true,"effective":"next_turn"})))
}
pub async fn remove_file(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id, file)): Path<(Uuid, Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.tx(org).await?;
    context_in(&mut tx, auth.user_id, org).await?;
    owned(&mut tx, org, auth.user_id, id, true).await?;
    sqlx::query("DELETE FROM project_files WHERE org_id=$1 AND project_id=$2 AND id=$3")
        .bind(org)
        .bind(id)
        .bind(file)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"deleted":true})))
}
#[derive(Deserialize)]
pub struct Move {
    pub project_id: Option<Uuid>,
}
pub async fn move_conversation(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Json(input): Json<Move>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.tx(org).await?;
    let ctx = context_in(&mut tx, auth.user_id, org).await?;
    conversation_in(&mut tx, &ctx, conv).await?;
    allowed(&app, &mut tx, org).await?;
    if let Some(project) = input.project_id {
        owned(&mut tx, org, auth.user_id, project, true).await?;
    }
    idle(&mut tx, org, conv).await?;
    // Deletion may have committed while we waited for the conversation channel.
    conversation_in(&mut tx, &ctx, conv).await?;
    let old:Option<String>=sqlx::query_scalar("SELECT p.memory_mode FROM projects p JOIN conversations c ON c.org_id=p.org_id AND c.project_id=p.id WHERE c.org_id=$1 AND c.id=$2").bind(org).bind(conv).fetch_optional(&mut *tx).await?;
    sqlx::query("UPDATE conversations SET project_id=$3 WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(conv)
        .bind(input.project_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(
        json!({"moved":true,"memory_notice":if old.as_deref()==Some("project_only"){Some("已有记忆保留在原项目内")}else{None}}),
    ))
}
pub async fn idle(tx: &mut Tx<'_>, org: Uuid, conv: Uuid) -> anyhow::Result<()> {
    sqlx::query("SELECT id FROM channels WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE")
        .bind(org)
        .bind(conv)
        .fetch_one(&mut **tx)
        .await?;
    let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('running','requires_action')) OR EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2 AND kind IN ('archive','restore') AND state IN ('queued','running')) OR EXISTS(SELECT 1 FROM conversation_operations WHERE org_id=$1 AND conversation_id=$2 AND state IN ('queued','running'))").bind(org).bind(conv).fetch_one(&mut **tx).await?;
    require(!busy, "CONVERSATION_BUSY")?;
    Ok(())
}
pub async fn delete(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.tx(org).await?;
    context_in(&mut tx, auth.user_id, org).await?;
    owned(&mut tx, org, auth.user_id, id, true).await?;
    let convs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM conversations WHERE org_id=$1 AND project_id=$2 ORDER BY id",
    )
    .bind(org)
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    for conv in convs {
        idle(&mut tx, org, conv).await?;
        let checkpointing: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('queued','running'))")
            .bind(org).bind(conv).fetch_one(&mut *tx).await?;
        require(!checkpointing, "WORKSPACE_BUSY")?;
    }
    sqlx::query("UPDATE conversations SET project_deleted_state=state,state='deletion_pending' WHERE org_id=$1 AND project_id=$2 AND state IN ('active','archived')").bind(org).bind(id).execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE projects SET state='deletion_pending',deleted_at=now() WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({"state":"deletion_pending","recoverable_days":30}),
    ))
}
pub async fn restore(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.tx(org).await?;
    context_in(&mut tx, auth.user_id, org).await?;
    owned(&mut tx, org, auth.user_id, id, false).await?;
    let ok=sqlx::query("UPDATE projects SET state='active',deleted_at=NULL WHERE org_id=$1 AND id=$2 AND state='deletion_pending' AND deleted_at>now()-interval '30 days'").bind(org).bind(id).execute(&mut *tx).await?.rows_affected();
    require(ok == 1, "RESTORE_EXPIRED")?;
    sqlx::query("UPDATE conversations SET state=project_deleted_state,project_deleted_state=NULL WHERE org_id=$1 AND project_id=$2 AND project_deleted_state IS NOT NULL").bind(org).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"restored":true})))
}
pub async fn instructions(tx: &mut Tx<'_>, org: Uuid, run: Uuid) -> anyhow::Result<String> {
    Ok(
        sqlx::query_scalar("SELECT project_context FROM runs WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(run)
            .fetch_one(&mut **tx)
            .await?,
    )
}
pub async fn search(tx: &mut Tx<'_>, org: Uuid, run: Uuid, query: &str) -> anyhow::Result<Value> {
    require(query.len() <= 256, "INVALID_TOOL_ARGUMENTS")?;
    let rows:Vec<(String,String)>=sqlx::query_as("SELECT f.key,f.value FROM runs r CROSS JOIN LATERAL jsonb_each_text(r.project_files) f WHERE r.org_id=$1 AND r.id=$2 ORDER BY f.key").bind(org).bind(run).fetch_all(&mut **tx).await?;
    let mut matches = vec![];
    let q = query.to_lowercase();
    for (name, content) in rows {
        for (n, line) in content.lines().enumerate() {
            if line.to_lowercase().contains(&q) {
                matches.push(json!({"file":name,"line":n+1,"text":line.chars().take(1500).collect::<String>()}));
                if matches.len() >= 20 {
                    return Ok(json!({"matches":matches}));
                }
            }
        }
    }
    Ok(json!({"matches":matches}))
}
pub async fn mount(
    app: &App,
    lease: &crate::store::Lease,
    run: Uuid,
    sandbox: Uuid,
) -> anyhow::Result<()> {
    let mut tx = app.store.tx(lease.org_id).await?;
    crate::chat::lock(&mut tx, lease).await?;
    let rows:Vec<(String,String)>=sqlx::query_as("SELECT f.key,f.value FROM runs r CROSS JOIN LATERAL jsonb_each_text(r.project_files) f WHERE r.org_id=$1 AND r.id=$2 ORDER BY f.key").bind(lease.org_id).bind(run).fetch_all(&mut *tx).await?;
    let instructions = instructions(&mut tx, lease.org_id, run).await?;
    tx.commit().await?;
    crate::sandbox::host(
        app,
        ryzhub_execution::HostCommand::Project {
            sandbox_id: sandbox,
            exec_epoch: lease.epoch as u64,
            files: rows.into_iter().collect(),
        },
    )
    .await?;
    crate::sandbox::guest(
        app,
        lease,
        sandbox,
        ryzhub_execution::GuestCommand::ProjectInstructions { text: instructions },
    )
    .await?;
    Ok(())
}

fn require(ok: bool, code: &str) -> anyhow::Result<()> {
    ensure!(ok, "{code}");
    Ok(())
}

/// Expiry holds the same project/channel locks as restore and command admission.
/// IDs and accounting remain as tombstones; user content and execution storage are reclaimed.
pub async fn collect(app: &App, org: Uuid, project: Uuid) -> anyhow::Result<bool> {
    use ryzhub_execution::HostCommand;
    let mut tx = app.store.tx(org).await?;
    let expired: Option<Uuid> = sqlx::query_scalar("SELECT id FROM projects WHERE org_id=$1 AND id=$2 AND state='deletion_pending' AND deleted_at<=now()-interval '30 days' FOR UPDATE")
        .bind(org).bind(project).fetch_optional(&mut *tx).await?;
    if expired.is_none() {
        return Ok(false);
    }
    let convs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM conversations WHERE org_id=$1 AND project_id=$2 ORDER BY id",
    )
    .bind(org)
    .bind(project)
    .fetch_all(&mut *tx)
    .await?;
    for conv in convs {
        idle(&mut tx, org, conv).await?;
        let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('queued','running'))")
            .bind(org).bind(conv).fetch_one(&mut *tx).await?;
        require(!busy, "WORKSPACE_BUSY")?;
        let epoch: i64 = sqlx::query_scalar("UPDATE channels SET writer_epoch=writer_epoch+1,owner_id=NULL,lease_until=NULL,published_seq=last_seq WHERE org_id=$1 AND conversation_id=$2 RETURNING writer_epoch")
            .bind(org).bind(conv).fetch_one(&mut *tx).await?;
        let sandboxes: Vec<(Uuid, i64)> = sqlx::query_as(
            "SELECT id,exec_epoch FROM sandboxes WHERE org_id=$1 AND conversation_id=$2",
        )
        .bind(org)
        .bind(conv)
        .fetch_all(&mut *tx)
        .await?;
        for (sandbox_id, old_epoch) in &sandboxes {
            crate::sandbox::host(
                app,
                HostCommand::Destroy {
                    sandbox_id: *sandbox_id,
                    exec_epoch: epoch.max(*old_epoch + 1) as u64,
                },
            )
            .await?;
        }
        if !sandboxes.is_empty() {
            let artifacts: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM checkpoints WHERE org_id=$1 AND conversation_id=$2 UNION SELECT id FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2")
                .bind(org).bind(conv).fetch_all(&mut *tx).await?;
            crate::sandbox::host(
                app,
                HostCommand::PurgeWorkspace {
                    org_id: org,
                    workspace_id: conv,
                    artifacts,
                },
            )
            .await?;
        }
        // All content tables below refer to immutable run IDs; no user-derived SQL identifiers.
        for table in [
            "pending_inputs",
            "loop_steps",
            "native_requests",
            "mcp_calls",
            "tool_calls",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table} WHERE org_id=$1 AND run_id IN (SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2)")))
                .bind(org).bind(conv).execute(&mut *tx).await?;
        }
        sqlx::query("DELETE FROM pending_actions WHERE org_id=$1 AND run_id IN (SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2)").bind(org).bind(conv).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM guest_events WHERE org_id=$1 AND sandbox_id IN (SELECT id FROM sandboxes WHERE org_id=$1 AND conversation_id=$2)").bind(org).bind(conv).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM events WHERE org_id=$1 AND channel_id=$2")
            .bind(org)
            .bind(format!("conv:{conv}"))
            .execute(&mut *tx)
            .await?;
        for table in [
            "commands",
            "conversation_operations",
            "workspace_jobs",
            "workspace_heads",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE org_id=$1 AND conversation_id=$2"
            )))
            .bind(org)
            .bind(conv)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("DELETE FROM restore_pins WHERE org_id=$1 AND id=(SELECT (native_fork->>'checkpoint_id')::uuid FROM conversations WHERE org_id=$1 AND id=$2)").bind(org).bind(conv).execute(&mut *tx).await?;
        sqlx::query("UPDATE checkpoints SET state='tombstoned',delete_after=now() WHERE org_id=$1 AND conversation_id=$2 AND state<>'deleted'").bind(org).bind(conv).execute(&mut *tx).await?;
        sqlx::query("UPDATE virtual_keys SET revoked_at=coalesce(revoked_at,now()) WHERE org_id=$1 AND run_id IN (SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2)").bind(org).bind(conv).execute(&mut *tx).await?;
        sqlx::query("UPDATE items SET text='',state='invalidated',revision=revision+1 WHERE org_id=$1 AND conversation_id=$2").bind(org).bind(conv).execute(&mut *tx).await?;
        sqlx::query("UPDATE runs SET response_text='',seed_messages='[]',project_context='',project_files='{}' WHERE org_id=$1 AND conversation_id=$2").bind(org).bind(conv).execute(&mut *tx).await?;
        sqlx::query("UPDATE sandboxes SET state='deleted',exec_epoch=$3 WHERE org_id=$1 AND conversation_id=$2").bind(org).bind(conv).bind(epoch).execute(&mut *tx).await?;
        sqlx::query("UPDATE conversations SET state='deleted',title='',context_seed=NULL,native_fork=NULL,project_deleted_state=NULL WHERE org_id=$1 AND id=$2").bind(org).bind(conv).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM project_files WHERE org_id=$1 AND project_id=$2")
        .bind(org)
        .bind(project)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE projects SET state='deleted',name='',instructions='',defaults='{}' WHERE org_id=$1 AND id=$2").bind(org).bind(project).execute(&mut *tx).await?;
    tx.commit().await?;
    crate::checkpoint::collect_org(app, org).await?;
    Ok(true)
}
pub async fn collector(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    loop {
        let projects: Vec<(Uuid,Uuid)> = sqlx::query_as("SELECT org_id,id FROM projects WHERE state='deletion_pending' AND deleted_at<=now()-interval '30 days' ORDER BY deleted_at LIMIT 100").fetch_all(&worker).await?;
        for (org, project) in projects {
            if let Err(error) = collect(&app, org, project).await {
                tracing::warn!(%org,%project,%error,"project collection deferred");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}
