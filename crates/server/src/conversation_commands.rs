//! Conversation-level commands: settings, compaction, fork, rename/archive and export.
//! Commands use the durable cid/Owner path; external effects run after commit.
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use ryzhub_proto::{Item, event_envelope::Event};
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
    crate::projects::idle(tx, lease.org_id, lease.conversation_id).await?;
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
    })))
}

#[derive(Deserialize)]
pub struct Platform {
    title: Option<String>,
    archive: Option<bool>,
}

/// `PATCH /orgs/{org}/conversations/{conv}/settings`: rename or (un)archive. Chat only for
/// archiving; a Work workspace is archived through its workspace job.
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
    let changed = input.title.is_some() || input.archive.is_some();
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
        crate::projects::idle(&mut tx, org, conv).await?;
        require!(c.mode != "work", Code::UseWorkspaceArchive);
        sqlx::query("UPDATE conversations SET state=$3 WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(conv)
            .bind(if archive { "archived" } else { "active" })
            .execute(&mut *tx)
            .await?;
    }
    if changed {
        let (title, state): (String, String) =
            sqlx::query_as("SELECT title,state FROM conversations WHERE org_id=$1 AND id=$2")
                .bind(org)
                .bind(conv)
                .fetch_one(&mut *tx)
                .await?;
        let channel = format!("user:{org}:{}", c.creator_id);
        // A user-channel sequence is also a monotonic metadata revision. Lock
        // before allocating it so concurrent conversations cannot reuse it.
        let revision: i64 = sqlx::query_scalar(
            "SELECT last_seq+1 FROM channels WHERE org_id=$1 AND id=$2 FOR UPDATE",
        )
        .bind(org)
        .bind(&channel)
        .fetch_one(&mut *tx)
        .await?;
        append(
            &mut tx,
            org,
            &channel,
            None,
            "",
            Event::ItemUpserted(Item {
                id: conv.to_string(),
                revision: revision as u64,
                kind: "conversation".into(),
                state,
                text: title,
                ..Default::default()
            }),
        )
        .await?;
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
        "SELECT jsonb_build_object('kind',kind,'state',state,'text',text,'seq',first_seq)
         FROM items WHERE org_id=$1 AND conversation_id=$2 ORDER BY first_seq",
    )
    .bind(org)
    .bind(conv)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"conversation":c,"items":items})))
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
    mode: String,
    title: String,
    run_config: Value,
    project_id: Option<Uuid>,
    model_credential_id: Option<Uuid>,
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
        "SELECT creator_id,mode,title,run_config,project_id,model_credential_id FROM conversations
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
        "INSERT INTO conversations(id,org_id,creator_id,title,mode,run_config,project_id,model_credential_id,
             context_seed,native_fork,owner_epoch)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,1)",
    )
    .bind(id)
    .bind(lease.org_id)
    .bind(source.creator_id)
    .bind(&title)
    .bind(&source.mode)
    .bind(&source.run_config)
    .bind(source.project_id)
    .bind(source.model_credential_id)
    .bind(seed)
    .bind((!native.is_null()).then_some(native))
    .execute(&mut *tx)
    .await?;
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
    let items: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT kind,state,text FROM items WHERE org_id=$1 AND conversation_id=$2 AND last_seq<=$3
         AND state='completed' ORDER BY first_seq",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(payload["seq"].as_i64().context(Code::InvalidFork)?)
    .fetch_all(&mut *tx)
    .await?;
    for (kind, state, text) in items {
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
    append(
        &mut tx,
        lease.org_id,
        &format!("user:{}:{}", lease.org_id, source.creator_id),
        None,
        "",
        Event::ItemUpserted(Item {
            id: id.to_string(),
            revision: 1,
            kind: "conversation".into(),
            state: "active".into(),
            text: title,
            ..Default::default()
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}
