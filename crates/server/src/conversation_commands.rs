//! Conversation-level commands: settings, compaction, fork, rename/archive, export and deletion.
//! Commands use the durable cid/Owner path; external effects run after commit.
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use ryzhub_execution::HostCommand;
use ryzhub_proto::{Item, ItemDetail, event_envelope::Event};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::App,
    config::Config,
    error::{ApiResult, Code},
    owner::OwnerLink,
    run_config::{self, RunConfig},
    store::{Lease, Tx, append, conversation_in, effective_policy},
};

/// `UpdateRunConfig`: merge a settings patch into the conversation's run configuration.
/// Settings changes are silent; the next run simply uses them.
pub async fn configure(
    tx: &mut Tx<'_>,
    lease: &Lease,
    input: &str,
    config: &Config,
) -> anyhow::Result<()> {
    ensure!(input.len() <= 4096, Code::InvalidConfig);
    let (mode, mut old): (String, Value) =
        sqlx::query_as("SELECT mode,run_config FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut **tx)
            .await?;
    let previous = RunConfig::resolve(old.clone(), &mode, config)?;
    let patch: Value = serde_json::from_str(input).map_err(|_| Code::InvalidConfig)?;
    let patch = patch.as_object().context(Code::InvalidConfig)?;
    // A model switch re-derives the wire protocol from the new model's provider.
    if patch.contains_key("model")
        && !patch.contains_key("protocol")
        && let Some(stored) = old.as_object_mut()
    {
        stored.remove("protocol");
    }
    for (k, v) in patch {
        old[k] = v.clone();
    }
    let current = RunConfig::resolve(old, &mode, config)?;
    // A fork's context seed is provider-native history, so it pins the protocol too.
    let has_runs: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM runs WHERE org_id=$1 AND conversation_id=$2)
             OR EXISTS(SELECT 1 FROM conversations WHERE org_id=$1 AND id=$2 AND context_seed IS NOT NULL)",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut **tx)
    .await?;
    ensure!(
        !has_runs || (previous.harness == current.harness && previous.protocol == current.protocol),
        Code::HarnessProtocolImmutable
    );
    current.validate(tx, lease.org_id, config).await?;
    if current.harness == "codex" && current.model != previous.model {
        let caps = native_state(tx, lease.org_id, lease.conversation_id)
            .await?
            .map(|(capabilities, _)| capabilities);
        if let Some(list) = caps.as_ref().and_then(|c| c["models"].as_array())
            && !list.is_empty()
        {
            let model = list
                .iter()
                .find(|m| m["id"] == current.model)
                .context(Code::ModelUnsupported)?;
            let levels = model["reasoning_levels"].as_array();
            ensure!(
                levels.is_none_or(|l| l.is_empty() || l.iter().any(|v| v == &current.reasoning)),
                Code::InvalidReasoning
            );
        }
    }
    sqlx::query("UPDATE conversations SET run_config=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .bind(serde_json::to_value(&current)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Capabilities and commands reported by the conversation's native harness, if it has run.
async fn native_state(
    tx: &mut Tx<'_>,
    org: Uuid,
    conv: Uuid,
) -> anyhow::Result<Option<(Value, Value)>> {
    Ok(sqlx::query_as(
        "SELECT capabilities,commands FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(org)
    .bind(conv)
    .fetch_optional(&mut **tx)
    .await?)
}

/// Compaction must be supported natively by the conversation's path.
pub async fn ensure_compactable(
    tx: &mut Tx<'_>,
    lease: &Lease,
    config: &Config,
) -> anyhow::Result<()> {
    let (mode, value): (String, Value) =
        sqlx::query_as("SELECT mode,run_config FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut **tx)
            .await?;
    let settings = RunConfig::resolve(value, &mode, config)?;
    let compact = &run_config::capabilities(&settings.harness, &settings.protocol)["compact"];
    ensure!(compact != "unsupported", Code::CompactUnsupported);
    Ok(())
}

/// `Fork`: queue a fork after the completed assistant reply `item`. The Owner's job runner
/// performs it (see [`jobs`]); `cid` becomes the new conversation's id.
pub async fn enqueue_fork(
    tx: &mut Tx<'_>,
    lease: &Lease,
    item: &str,
    cid: &str,
) -> anyhow::Result<()> {
    idle(tx, lease.org_id, lease.conversation_id).await?;
    let item = Uuid::parse_str(item).map_err(|_| Code::InvalidFork)?;
    let (seq, run, turn, harness): (i64, Uuid, Option<String>, String) = sqlx::query_as(
        "SELECT i.last_seq,i.run_id,r.native_turn_id,r.harness FROM items i
         JOIN runs r ON r.org_id=i.org_id AND r.id=i.run_id
         WHERE i.org_id=$1 AND i.conversation_id=$2 AND i.id=$3 AND i.kind='assistant' AND i.state='completed'
           AND r.state='completed'",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(item)
    .fetch_optional(&mut **tx)
    .await?
    .context(Code::ForkPointUnavailable)?;
    let message: Option<String> = sqlx::query_scalar(
        "SELECT native_message_id FROM native_items WHERE org_id=$1 AND item_id=$2",
    )
    .bind(lease.org_id)
    .bind(item)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    ensure!(
        run_config::capabilities(&harness, "")["fork"] != "unsupported",
        Code::ForkPointUnavailable
    );
    match harness.as_str() {
        "claude" => ensure!(message.is_some(), Code::ForkPointUnavailable),
        "codex" => {
            let points =
                codex_fork_points(tx, lease.org_id, lease.conversation_id, Some(item)).await?;
            ensure!(!points.is_empty(), Code::ForkPointUnavailable);
        }
        _ => {}
    }
    let payload = json!({
        "item_id": item,
        "seq": seq,
        "run_id": run,
        "turn_id": turn,
        "message_id": message,
    });
    sqlx::query(
        "INSERT INTO conversation_operations(id,org_id,conversation_id,kind,payload) VALUES($1,$2,$3,'fork',$4)",
    )
    .bind(Uuid::parse_str(cid)?)
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(payload)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Codex can truncate after a turn, not after an intermediate assistant message.
/// Require a native reply with no later non-reasoning item in that completed run.
async fn codex_fork_points(
    tx: &mut Tx<'_>,
    org: Uuid,
    conv: Uuid,
    item: Option<Uuid>,
) -> anyhow::Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT i.id FROM items i JOIN runs r ON r.org_id=i.org_id AND r.id=i.run_id
         WHERE i.org_id=$1 AND i.conversation_id=$2 AND ($3::uuid IS NULL OR i.id=$3)
           AND i.kind='assistant' AND i.state='completed' AND r.state='completed'
           AND r.harness='codex' AND r.native_turn_id IS NOT NULL
           AND EXISTS(SELECT 1 FROM native_items n WHERE n.org_id=i.org_id AND n.item_id=i.id)
           AND NOT EXISTS(SELECT 1 FROM items later WHERE later.org_id=i.org_id
               AND later.run_id=i.run_id AND later.last_seq>i.last_seq
               AND later.kind<>'reasoning' AND later.state<>'cancelled')",
    )
    .bind(org)
    .bind(conv)
    .bind(item)
    .fetch_all(&mut **tx)
    .await?)
}

/// `GET /orgs/{org}/conversations/{conv}/commands`: what the conversation's settings panel
/// may offer now: models, reasoning levels, approval modes, native commands and forks.
pub async fn catalog(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    let c = conversation_in(&mut tx, &ctx, conv).await?;
    let settings = RunConfig::resolve(c.run_config, &c.mode, &app.config)?;
    let policy = effective_policy(&mut tx, &app.config.policy, org).await?;
    let native = native_state(&mut tx, org, conv).await?;
    let mut models = run_config::models(&app.config, &settings.harness, &settings.protocol);
    models.retain(|m| policy.models.as_ref().is_none_or(|p| p.contains(m)));
    // Agent SDK accepts explicit model IDs through setModel. Its discovery list
    // contains convenience aliases, so explicit administrator routes remain valid.
    if settings.harness == "codex"
        && let Some((caps, _)) = &native
        && let Some(list) = caps["models"].as_array()
        && !list.is_empty()
    {
        models.retain(|m| list.iter().any(|n| n["id"] == *m));
    }
    // Chat over Anthropic keeps the full list; clients gate it on `thought_level`.
    let mut reasoning = match run_config::reasoning_levels(&settings.harness, &settings.protocol) {
        levels if levels.is_empty() => vec!["low", "medium", "high", "xhigh"],
        levels => levels,
    };
    if settings.harness == "codex"
        && let Some((caps, _)) = &native
        && let Some(model) = caps["models"]
            .as_array()
            .and_then(|l| l.iter().find(|m| m["id"] == settings.model))
        && let Some(levels) = model["reasoning_levels"].as_array()
        && !levels.is_empty()
    {
        reasoning.retain(|r| levels.iter().any(|v| v == r));
    }
    let operations: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',id,'kind',kind,'state',state,'result',result)
         FROM conversation_operations WHERE org_id=$1 AND conversation_id=$2 ORDER BY created_at DESC LIMIT 10",
    )
    .bind(org)
    .bind(conv)
    .fetch_all(&mut *tx)
    .await?;
    let fork_points = if settings.harness == "codex" {
        Some(codex_fork_points(&mut tx, org, conv, None).await?)
    } else {
        None
    };
    // The commands of the plugins on here, for the `/` menu (InvokeMenuCommand).
    let plugin_commands = crate::plugins::commands(&mut tx, org, conv).await?;
    tx.commit().await?;
    let approval_modes = if policy.allow_full_auto == Some(false) {
        vec!["ask", "allow_edits"]
    } else {
        vec!["ask", "allow_edits", "full_auto"]
    };
    Ok(Json(json!({
        "provider": run_config::provider(&settings.protocol),
        "capabilities": run_config::capabilities(&settings.harness, &settings.protocol),
        "settings": settings,
        "models": models,
        "reasoning_levels": reasoning,
        "approval_modes": approval_modes,
        "effective": "next_turn",
        "harness_commands": native.map_or(json!([]), |(_, commands)| commands),
        "operations": operations,
        "fork_points": fork_points,
        "plugin_commands": plugin_commands,
    })))
}

#[derive(Deserialize)]
pub struct Platform {
    title: Option<String>,
    archive: Option<bool>,
    pinned: Option<bool>,
}

/// `PATCH /orgs/{org}/conversations/{conv}/settings`: rename, (un)archive or (un)pin. Chat
/// only for archiving; a Work workspace is archived through its workspace job. Pinning again
/// keeps the original pin time; a temporary conversation cannot be pinned.
pub async fn platform(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Json(input): Json<Platform>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    // Serialize metadata with deletion and command admission, including rename
    // during an active run. Revalidate access after any wait on this lock.
    sqlx::query("SELECT id FROM channels WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE")
        .bind(org)
        .bind(conv)
        .fetch_optional(&mut *tx)
        .await?
        .context(Code::Forbidden)?;
    let c = conversation_in(&mut tx, &ctx, conv).await?;
    let changed = input.title.is_some() || input.archive.is_some() || input.pinned.is_some();
    if let Some(title) = input.title {
        require!(
            !title.trim().is_empty() && title.len() <= 256,
            Code::InvalidConversation
        );
        sqlx::query("UPDATE conversations SET title=$3 WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(conv)
            .bind(title)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(archive) = input.archive {
        idle(&mut tx, org, conv).await?;
        require!(c.mode != "work", Code::UseWorkspaceArchive);
        if archive {
            crate::lifecycle::archive(&mut tx, org, conv).await?;
        } else {
            crate::lifecycle::unarchive(&mut tx, org, conv, "hot").await?;
        }
    }
    if let Some(pinned) = input.pinned {
        require!(!(pinned && c.temporary), Code::InvalidConversation);
        sqlx::query(
            "UPDATE conversations SET pinned_at=CASE WHEN $3 THEN COALESCE(pinned_at,now()) END
             WHERE org_id=$1 AND id=$2",
        )
        .bind(org)
        .bind(conv)
        .bind(pinned)
        .execute(&mut *tx)
        .await?;
    }
    if changed {
        crate::store::publish_conversation(&mut tx, org, conv).await?;
    }
    tx.commit().await?;
    Ok(Json(json!({"updated":true})))
}

/// `GET /orgs/{org}/conversations/{conv}/export`: the conversation and its timeline.
pub async fn export(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    let c = conversation_in(&mut tx, &ctx, conv).await?;
    let items: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('kind',kind,'state',state,'text',text,'seq',first_seq,'detail',detail)
         FROM items WHERE org_id=$1 AND conversation_id=$2 ORDER BY first_seq",
    )
    .bind(org)
    .bind(conv)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"conversation":c,"items":items})))
}

/// Lock the conversation's channel (serializing with command admission) and require that
/// nothing is running in it.
pub async fn idle(tx: &mut Tx<'_>, org: Uuid, conv: Uuid) -> anyhow::Result<()> {
    sqlx::query("SELECT id FROM channels WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE")
        .bind(org)
        .bind(conv)
        .fetch_one(&mut **tx)
        .await?;
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM runs WHERE org_id=$1 AND conversation_id=$2
             AND state IN ('running','requires_action'))
           OR EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2
             AND kind IN ('archive','restore') AND state IN ('queued','running'))
           OR EXISTS(SELECT 1 FROM conversation_operations WHERE org_id=$1 AND conversation_id=$2
             AND state IN ('queued','running'))",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&mut **tx)
    .await?;
    ensure!(!busy, Code::ConversationBusy);
    Ok(())
}

/// [`idle`], and no workspace job either: what deleting the conversation needs.
pub async fn deletable(tx: &mut Tx<'_>, org: Uuid, conv: Uuid) -> anyhow::Result<()> {
    idle(tx, org, conv).await?;
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2
           AND state IN ('queued','running'))",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&mut **tx)
    .await?;
    ensure!(!busy, Code::WorkspaceBusy);
    Ok(())
}

#[derive(Default, Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    pub delete_memories: bool,
}

/// `DELETE /orgs/{org}/conversations/{conv}?delete_memories=`: hide the conversation at once
/// (and the memories it saved, when asked); [`collector`] purges its content.
pub async fn delete(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult<StatusCode> {
    let (ctx, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    sqlx::query("SELECT id FROM channels WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE")
        .bind(org)
        .bind(conv)
        .fetch_optional(&mut *tx)
        .await?
        .context(Code::Forbidden)?;
    conversation_in(&mut tx, &ctx, conv).await?;
    deletable(&mut tx, org, conv).await?;
    sqlx::query(
        "UPDATE conversations SET state='deleted',memory_snapshot=NULL WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(conv)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM conversation_digests WHERE org_id=$1 AND conversation_id=$2")
        .bind(org)
        .bind(conv)
        .execute(&mut *tx)
        .await?;
    if q.delete_memories {
        sqlx::query(
            "UPDATE memory_items SET status='deleted',invalid_at=now(),version=version+1
             WHERE org_id=$1 AND user_id=$2 AND source_conversation_id=$3 AND status='active'",
        )
        .bind(org)
        .bind(ctx.user_id)
        .bind(conv)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE virtual_keys SET revoked_at=now()
         WHERE org_id=$1 AND conversation_id=$2 AND revoked_at IS NULL",
    )
    .bind(org)
    .bind(conv)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Owner-role loop purging deleted and expired temporary conversations.
pub async fn collector(app: App, worker: sqlx::PgPool) -> anyhow::Result<()> {
    loop {
        let due: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT org_id,id FROM conversations
             WHERE title<>'' AND (state='deleted' OR (temporary AND expires_at<=now())) LIMIT 100",
        )
        .fetch_all(&worker)
        .await?;
        for (org, conv) in due {
            if let Err(error) = collect(&app, org, conv).await {
                tracing::warn!(conversation = %conv, %error, "conversation deletion deferred");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

/// Purge `conv` if it is deleted or an expired temporary conversation and not yet purged.
pub async fn collect(app: &App, org: Uuid, conv: Uuid) -> anyhow::Result<bool> {
    let mut tx = app.store.tx(org).await?;
    sqlx::query("SELECT id FROM channels WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE")
        .bind(org)
        .bind(conv)
        .execute(&mut *tx)
        .await?;
    let due: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM conversations WHERE org_id=$1 AND id=$2
           AND (state='deleted' OR (temporary AND expires_at<=now())) AND title<>'')",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&mut *tx)
    .await?;
    if !due {
        return Ok(false);
    }
    purge(app, &mut tx, org, conv).await?;
    tx.commit().await?;
    Ok(true)
}

/// Purge a conversation nothing works in: fence its writers, destroy its sandboxes and stored
/// workspace, and delete its content, keeping ids and accounting rows as tombstones.
pub async fn purge(app: &App, tx: &mut Tx<'_>, org: Uuid, conv: Uuid) -> anyhow::Result<()> {
    deletable(tx, org, conv).await?;
    // Its events go with the content: nothing is left to publish.
    let epoch: i64 = sqlx::query_scalar(
        "UPDATE channels SET writer_epoch=writer_epoch+1,owner_id=NULL,lease_until=NULL,
           published_seq=last_seq
         WHERE org_id=$1 AND conversation_id=$2 RETURNING writer_epoch",
    )
    .bind(org)
    .bind(conv)
    .fetch_one(&mut **tx)
    .await?;
    let sandboxes: Vec<(Uuid, i64, bool)> = sqlx::query_as(
        "SELECT id,exec_epoch,state='deleted' FROM sandboxes WHERE org_id=$1 AND conversation_id=$2",
    )
    .bind(org)
    .bind(conv)
    .fetch_all(&mut **tx)
    .await?;
    // A deleted sandbox is gone; its workspace files may not be.
    for (sandbox_id, old_epoch, _) in sandboxes.iter().filter(|(.., deleted)| !deleted) {
        let destroy = HostCommand::Destroy {
            sandbox_id: *sandbox_id,
            exec_epoch: epoch.max(*old_epoch + 1) as u64,
        };
        crate::sandbox::host(app, org, *sandbox_id, destroy).await?;
    }
    if !sandboxes.is_empty() {
        let artifacts: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM checkpoints WHERE org_id=$1 AND conversation_id=$2
             UNION SELECT id FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2",
        )
        .bind(org)
        .bind(conv)
        .fetch_all(&mut **tx)
        .await?;
        let workspace = HostCommand::PurgeWorkspace {
            org_id: org,
            workspace_id: conv,
            artifacts,
        };
        crate::sandbox::host_workspace(app, org, conv, workspace).await?;
    }
    erase(tx, org, conv, epoch).await
}

/// Delete a conversation's content, keeping ids and accounting rows as tombstones.
async fn erase(tx: &mut Tx<'_>, org: Uuid, conv: Uuid, epoch: i64) -> anyhow::Result<()> {
    // Every statement is scoped by ($1 org, $2 conversation), in dependency order. Table names
    // are fixed identifiers, never input.
    let runs = "SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2";
    let mut statements = vec![
        // Seed pointers reference loop steps about to be deleted.
        "UPDATE runs SET seed_run_id=NULL,seed_step=NULL WHERE org_id=$1 AND conversation_id=$2"
            .to_owned(),
    ];
    for table in [
        "pending_inputs",
        "loop_steps",
        "native_requests",
        "mcp_calls",
        "tool_calls",
        "pending_actions",
    ] {
        statements.push(format!(
            "DELETE FROM {table} WHERE org_id=$1 AND run_id IN ({runs})"
        ));
    }
    statements.push(
        "DELETE FROM guest_events WHERE org_id=$1
         AND sandbox_id IN (SELECT id FROM sandboxes WHERE org_id=$1 AND conversation_id=$2)"
            .into(),
    );
    for table in [
        "commands",
        "conversation_operations",
        "workspace_jobs",
        "workspace_heads",
        "chat_images",
        // References only: the files stay in their owner's library.
        "conversation_files",
    ] {
        statements.push(format!(
            "DELETE FROM {table} WHERE org_id=$1 AND conversation_id=$2"
        ));
    }
    statements.extend(
        [
            // A fork's restore pin is keyed by the forked conversation.
            "DELETE FROM conversation_digests WHERE org_id=$1 AND conversation_id=$2",
            "UPDATE conversations SET memory_snapshot=NULL,expires_at=NULL WHERE org_id=$1 AND id=$2",
            "DELETE FROM restore_pins WHERE org_id=$1 AND id=$2",
            "UPDATE checkpoints SET state='tombstoned',delete_after=now()
             WHERE org_id=$1 AND conversation_id=$2 AND state<>'deleted'",
            "UPDATE virtual_keys SET revoked_at=coalesce(revoked_at,now()) WHERE org_id=$1 AND conversation_id=$2",
            "UPDATE items SET text='',detail=NULL,state='invalidated',revision=revision+1
             WHERE org_id=$1 AND conversation_id=$2",
            "UPDATE runs SET response_text='',seed_messages='[]' WHERE org_id=$1 AND conversation_id=$2",
        ]
        .map(String::from),
    );
    for statement in statements {
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(org)
            .bind(conv)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("DELETE FROM search_docs WHERE org_id=$1 AND conversation_id=$2")
        .bind(org)
        .bind(conv)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM events WHERE org_id=$1 AND channel_id=$2")
        .bind(org)
        .bind(format!("conv:{conv}"))
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "UPDATE sandboxes SET state='deleted',exec_epoch=$3 WHERE org_id=$1 AND conversation_id=$2",
    )
    .bind(org)
    .bind(conv)
    .bind(epoch)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE conversations SET state='deleted',title='',context_seed=NULL,native_fork=NULL,project_deleted_state=NULL
         WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(conv)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Run the conversation's queued fork, if any.
pub async fn jobs(app: &App, lease: &Lease, link: &OwnerLink) -> anyhow::Result<()> {
    let mut tx = app.store.fenced(lease).await?;
    let row: Option<(Uuid, Value)> = sqlx::query_as(
        "SELECT id,payload FROM conversation_operations
         WHERE org_id=$1 AND conversation_id=$2 AND state IN ('queued','running')
         ORDER BY created_at LIMIT 1 FOR UPDATE",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, payload)) = row else {
        return Ok(());
    };
    sqlx::query("UPDATE conversation_operations SET state='running' WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let result = fork(app, lease, id, payload, link).await;
    if result.as_ref().is_err_and(crate::error::is_fenced) {
        return result.map(|_| ());
    }
    let (state, value, notice) = match &result {
        Ok(conv) => (
            "completed",
            json!({"conversation_id":conv}),
            "分叉对话已创建；文件基于当前工作区快照。",
        ),
        Err(error) => {
            tracing::warn!(fork = %id, error = format!("{error:#}"), "fork failed");
            (
                "failed",
                json!({"error":crate::error::public_code(error).as_str()}),
                "分叉失败，可查看命令状态后重试。",
            )
        }
    };
    let mut tx = app.store.fenced(lease).await?;
    sqlx::query("UPDATE conversation_operations SET state=$3,result=$4 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(id)
        .bind(state)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    append(
        &mut tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        &id.to_string(),
        Event::ItemUpserted(Item {
            id: id.to_string(),
            revision: 1,
            kind: "notice".into(),
            state: state.into(),
            text: notice.into(),
            ..Default::default()
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The conversation a fork copies its settings from.
#[derive(sqlx::FromRow)]
struct ForkSource {
    creator_id: Uuid,
    temporary: bool,
    mode: String,
    title: String,
    run_config: Value,
    project_id: Option<Uuid>,
}

/// Create the forked conversation `id`: a checkpoint of the workspace (Work) or the reply's
/// native context (Chat), plus a copy of the timeline up to the fork point. Idempotent.
async fn fork(
    app: &App,
    lease: &Lease,
    id: Uuid,
    payload: Value,
    link: &OwnerLink,
) -> anyhow::Result<Uuid> {
    let mut tx = app.store.fenced(lease).await?;
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM conversations WHERE org_id=$1 AND id=$2)",
    )
    .bind(lease.org_id)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?
    {
        return Ok(id);
    }
    let source: ForkSource = sqlx::query_as(
        "SELECT creator_id,mode,title,run_config,project_id,temporary FROM conversations
         WHERE org_id=$1 AND id=$2",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let checkpoint = if source.mode == "work" {
        // A suspended workspace forks from the checkpoint it was stopped with.
        Some(crate::workspace::current(app, lease, id, link).await?)
    } else {
        None
    };
    let mut tx = app.store.fenced(lease).await?;
    let mut seed = json!([]);
    let mut native = Value::Null;
    if let Some(checkpoint) = checkpoint {
        let manifest: Value =
            sqlx::query_scalar("SELECT manifest FROM checkpoints WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(checkpoint)
                .fetch_one(&mut *tx)
                .await?;
        native = json!({
            "checkpoint_id": checkpoint,
            "manifest": manifest,
            "last_turn_id": payload["turn_id"],
            "message_id": payload["message_id"],
        });
        // One pin per fork (keyed by the new conversation): forks of a suspended workspace
        // share its head checkpoint, and each must keep it until it has restored.
        sqlx::query(
            "INSERT INTO restore_pins(id,org_id,checkpoint_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(lease.org_id)
        .bind(checkpoint)
        .execute(&mut *tx)
        .await?;
    } else {
        let run = Uuid::parse_str(payload["run_id"].as_str().context(Code::InvalidFork)?)?;
        let item = Uuid::parse_str(payload["item_id"].as_str().context(Code::InvalidFork)?)?;
        let step: i32 = sqlx::query_scalar(
            "SELECT step_no FROM loop_steps WHERE org_id=$1 AND run_id=$2 AND item_id=$3 AND status='completed'",
        )
        .bind(lease.org_id)
        .bind(run)
        .bind(item)
        .fetch_optional(&mut *tx)
        .await?
        .context(Code::ForkPointUnavailable)?;
        // The reply's context, without the results of any later tool calls.
        seed = crate::chat::context::request_messages(&mut tx, lease.org_id, run, step).await?;
        let (protocol, response): (String, Value) = sqlx::query_as(
            "SELECT protocol,native_response FROM loop_steps WHERE org_id=$1 AND run_id=$2 AND step_no=$3",
        )
        .bind(lease.org_id)
        .bind(run)
        .bind(step)
        .fetch_one(&mut *tx)
        .await?;
        crate::chat::protocol::continue_with(&protocol, &mut seed, &response, &[])?;
    }
    let title = format!("{} · 分叉", source.title);
    sqlx::query(
        "INSERT INTO conversations(id,org_id,creator_id,title,mode,run_config,project_id,
             context_seed,native_fork,owner_epoch,temporary,expires_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,1,$10,CASE WHEN $10 THEN now()+interval '30 days' END)",
    )
    .bind(id)
    .bind(lease.org_id)
    .bind(source.creator_id)
    .bind(&title)
    .bind(&source.mode)
    .bind(&source.run_config)
    .bind(source.project_id)
    .bind(seed)
    .bind((!native.is_null()).then_some(native))
    .bind(source.temporary)
    .execute(&mut *tx)
    .await?;
    // A Chat seed refers to the images its model viewed and the files messages carried; the
    // files keep their names under /attachments.
    for statement in [
        "INSERT INTO chat_images(org_id,conversation_id,sha256,media_type,data,width,height)
         SELECT org_id,$3,sha256,media_type,data,width,height FROM chat_images
         WHERE org_id=$1 AND conversation_id=$2",
        "INSERT INTO conversation_files(org_id,conversation_id,attachment_id,path_name)
         SELECT org_id,$3,attachment_id,path_name FROM conversation_files
         WHERE org_id=$1 AND conversation_id=$2",
    ] {
        sqlx::query(statement)
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    let child = Lease {
        conversation_id: id,
        org_id: lease.org_id,
        owner_id: lease.owner_id,
        epoch: 1,
    };
    // The child channel is written under a short lease of its own, released below.
    sqlx::query(
        "INSERT INTO channels(id,org_id,kind,conversation_id,writer_epoch,owner_id,lease_until)
         VALUES($1,$2,'conv',$3,1,$4,now()+interval '30 seconds')",
    )
    .bind(child.channel())
    .bind(lease.org_id)
    .bind(id)
    .bind(lease.owner_id)
    .execute(&mut *tx)
    .await?;
    type Copied = (
        String,
        String,
        String,
        Option<sqlx::types::Json<ItemDetail>>,
    );
    let items: Vec<Copied> = sqlx::query_as(
        "SELECT kind,state,text,detail FROM items WHERE org_id=$1 AND conversation_id=$2 AND last_seq<=$3
         AND state='completed' ORDER BY first_seq",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(payload["seq"].as_i64().context(Code::InvalidFork)?)
    .fetch_all(&mut *tx)
    .await?;
    for (kind, state, text, detail) in items {
        append(
            &mut tx,
            lease.org_id,
            &child.channel(),
            Some(&child),
            "",
            Event::ItemUpserted(Item {
                id: Uuid::now_v7().to_string(),
                revision: 1,
                kind,
                state,
                text,
                detail: detail.map(|sqlx::types::Json(detail)| detail),
                ..Default::default()
            }),
        )
        .await?;
    }
    sqlx::query("UPDATE channels SET lease_until=now() WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(child.channel())
        .execute(&mut *tx)
        .await?;
    crate::store::publish_conversation(&mut tx, lease.org_id, id).await?;
    tx.commit().await?;
    Ok(id)
}
