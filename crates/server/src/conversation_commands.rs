//! Commands use the existing durable CID/Owner path; external effects run after commit.
use crate::{
    api::{ApiResult, App, authenticate},
    run_config::RunConfig,
    store::{Lease, Tx, append, context_in, conversation_in},
};
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use ryzhub_proto::{Item, event_envelope::Event};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

pub async fn configure(
    tx: &mut Tx<'_>,
    lease: &Lease,
    input: &str,
    config: &crate::config::Config,
    cid: &str,
) -> anyhow::Result<()> {
    ensure!(input.len() <= 4096, "INVALID_CONFIG");
    let (mode, mut old): (String, Value) =
        sqlx::query_as("SELECT mode,run_config FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut **tx)
            .await?;
    let previous = RunConfig::resolve(old.clone(), &mode, config)?;
    let patch: Value = serde_json::from_str(input)?;
    for (k, v) in patch.as_object().context("INVALID_CONFIG")? {
        old[k] = v.clone();
    }
    let current = RunConfig::resolve(old, &mode, config)?;
    let has_runs: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM runs WHERE org_id=$1 AND conversation_id=$2)",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut **tx)
    .await?;
    ensure!(
        !has_runs || (previous.harness == current.harness && previous.protocol == current.protocol),
        "HARNESS_PROTOCOL_IMMUTABLE"
    );
    current.validate(tx, lease.org_id, config).await?;
    if current.harness == "codex" && current.model != previous.model {
        let caps: Option<Value> = sqlx::query_scalar("SELECT capabilities FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent ORDER BY created_at DESC LIMIT 1")
            .bind(lease.org_id).bind(lease.conversation_id).fetch_optional(&mut **tx).await?;
        if let Some(list) = caps.as_ref().and_then(|c| c["models"].as_array())
            && !list.is_empty()
        {
            let model = list
                .iter()
                .find(|m| m["model"] == current.model || m["id"] == current.model)
                .context("MODEL_UNSUPPORTED")?;
            let levels = model["supportedReasoningEfforts"].as_array();
            ensure!(
                levels
                    .is_none_or(|l| l.is_empty()
                        || l.iter().any(|v| v["reasoningEffort"] == current.reasoning)),
                "INVALID_REASONING"
            );
        }
    }
    sqlx::query("UPDATE conversations SET run_config=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .bind(serde_json::to_value(&current)?)
        .execute(&mut **tx)
        .await?;
    append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        cid,
        Event::ItemUpserted(Item {
            id: Uuid::now_v7().to_string(),
            revision: 1,
            kind: "notice".into(),
            state: "completed".into(),
            text: format!(
                "设置已保存，下一轮生效：{} · {} · {}{}",
                current.model,
                current.reasoning,
                current.approval,
                if current.plan { " · 计划模式" } else { "" }
            ),
            ..Default::default()
        }),
    )
    .await?;
    Ok(())
}
pub async fn enqueue_fork(
    tx: &mut Tx<'_>,
    lease: &Lease,
    item: &str,
    cid: &str,
) -> anyhow::Result<()> {
    crate::projects::idle(tx, lease.org_id, lease.conversation_id).await?;
    let item = Uuid::parse_str(item)?;
    let row=sqlx::query("SELECT i.last_seq,i.run_id,r.native_turn_id,r.harness FROM items i JOIN runs r ON r.org_id=i.org_id AND r.id=i.run_id WHERE i.org_id=$1 AND i.conversation_id=$2 AND i.id=$3 AND i.kind='assistant' AND i.state='completed' AND r.state='completed'").bind(lease.org_id).bind(lease.conversation_id).bind(item).fetch_optional(&mut **tx).await?.context("FORK_POINT_UNAVAILABLE")?;
    let message: Option<String> = sqlx::query_scalar(
        "SELECT native_message_id FROM native_items WHERE org_id=$1 AND item_id=$2",
    )
    .bind(lease.org_id)
    .bind(item)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    if row.get::<&str, _>("harness") == "claude" {
        ensure!(message.is_some(), "FORK_POINT_UNAVAILABLE");
    }
    if row.get::<&str, _>("harness") == "codex" {
        ensure!(
            !codex_fork_points(tx, lease.org_id, lease.conversation_id, Some(item))
                .await?
                .is_empty(),
            "FORK_POINT_UNAVAILABLE"
        );
    }
    let payload = json!({"item_id":item,"seq":row.get::<i64,_>("last_seq"),"run_id":row.get::<Uuid,_>("run_id"),"turn_id":row.get::<Option<String>,_>("native_turn_id"),"message_id":message});
    sqlx::query("INSERT INTO conversation_operations(id,org_id,conversation_id,kind,payload) VALUES($1,$2,$3,'fork',$4)").bind(Uuid::parse_str(cid)?).bind(lease.org_id).bind(lease.conversation_id).bind(payload).execute(&mut **tx).await?;
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
pub async fn catalog(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.tx(org).await?;
    let ctx = context_in(&mut tx, auth.user_id, org).await?;
    let c = conversation_in(&mut tx, &ctx, conv).await?;
    let settings = RunConfig::resolve(c.run_config, &c.mode, &app.config)?;
    let policy = crate::llm::gateway::effective_policy(&app, &mut tx, org).await?;
    let native:Option<(Value,Value)>=sqlx::query_as("SELECT capabilities,commands FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent ORDER BY created_at DESC LIMIT 1").bind(org).bind(conv).fetch_optional(&mut *tx).await?;
    let mut models = crate::run_config::models(&app.config, &settings.harness, &settings.protocol);
    models.retain(|m| policy.models.as_ref().is_none_or(|p| p.contains(m)));
    // Agent SDK accepts explicit model IDs through setModel. Its discovery list
    // contains convenience aliases, so explicit administrator routes remain valid.
    if settings.harness == "codex"
        && let Some((caps, _)) = &native
        && let Some(list) = caps["models"].as_array()
        && !list.is_empty()
    {
        models.retain(|m| list.iter().any(|n| n["model"] == *m || n["id"] == *m));
    }
    let mut reasoning = if settings.harness == "claude" {
        vec!["low", "medium", "high", "xhigh", "max"]
    } else {
        vec!["low", "medium", "high", "xhigh"]
    };
    if settings.harness == "codex"
        && let Some((caps, _)) = &native
        && let Some(model) = caps["models"].as_array().and_then(|l| {
            l.iter()
                .find(|m| m["model"] == settings.model || m["id"] == settings.model)
        })
        && let Some(levels) = model["supportedReasoningEfforts"].as_array()
        && !levels.is_empty()
    {
        reasoning.retain(|r| levels.iter().any(|v| v["reasoningEffort"] == *r));
    }
    let operations:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'kind',kind,'state',state,'result',result) FROM conversation_operations WHERE org_id=$1 AND conversation_id=$2 ORDER BY created_at DESC LIMIT 10").bind(org).bind(conv).fetch_all(&mut *tx).await?;
    let fork_points = if settings.harness == "codex" {
        Some(codex_fork_points(&mut tx, org, conv, None).await?)
    } else {
        None
    };
    tx.commit().await?;
    Ok(Json(
        json!({"settings":settings,"capabilities":crate::run_config::capabilities(&settings.harness,&settings.protocol),"models":models,"reasoning_levels":reasoning,"approval_modes":if policy.allow_full_auto==Some(false){vec!["ask","allow_edits"]}else{vec!["ask","allow_edits","full_auto"]},"effective":"next_turn","harness_commands":native.map(|(_,commands)|commands).unwrap_or(json!([])),"operations":operations,"fork_points":fork_points}),
    ))
}
#[derive(Deserialize)]
pub struct Platform {
    title: Option<String>,
    archive: Option<bool>,
}
pub async fn platform(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Json(input): Json<Platform>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.tx(org).await?;
    let ctx = context_in(&mut tx, auth.user_id, org).await?;
    // Serialize metadata with deletion and command admission, including rename
    // during an active run. Revalidate access after any wait on this lock.
    sqlx::query("SELECT id FROM channels WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE")
        .bind(org)
        .bind(conv)
        .fetch_optional(&mut *tx)
        .await?
        .context("FORBIDDEN")?;
    let c = conversation_in(&mut tx, &ctx, conv).await?;
    let changed = input.title.is_some() || input.archive.is_some();
    if let Some(title) = input.title {
        if title.trim().is_empty() || title.len() > 256 {
            return Err(anyhow::anyhow!("INVALID_CONVERSATION").into());
        }
        sqlx::query("UPDATE conversations SET title=$3 WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(conv)
            .bind(title)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(archive) = input.archive {
        crate::projects::idle(&mut tx, org, conv).await?;
        if c.mode == "work" {
            return Err(anyhow::anyhow!("USE_WORKSPACE_ARCHIVE").into());
        }
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
pub async fn export(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.tx(org).await?;
    let ctx = context_in(&mut tx, auth.user_id, org).await?;
    let c = conversation_in(&mut tx, &ctx, conv).await?;
    let items:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('kind',kind,'state',state,'text',text,'seq',first_seq) FROM items WHERE org_id=$1 AND conversation_id=$2 ORDER BY first_seq").bind(org).bind(conv).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"conversation":c,"items":items})))
}
pub async fn jobs(app: &App, lease: &Lease) -> anyhow::Result<()> {
    let mut tx = app.store.tx(lease.org_id).await?;
    crate::chat::lock(&mut tx, lease).await?;
    let row:Option<(Uuid,Value)>=sqlx::query_as("SELECT id,payload FROM conversation_operations WHERE org_id=$1 AND conversation_id=$2 AND state IN ('queued','running') ORDER BY created_at LIMIT 1 FOR UPDATE").bind(lease.org_id).bind(lease.conversation_id).fetch_optional(&mut *tx).await?;
    let Some((id, payload)) = row else {
        return Ok(());
    };
    sqlx::query("UPDATE conversation_operations SET state='running' WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let result = fork(app, lease, id, payload).await;
    if result.as_ref().is_err_and(|e| e.to_string() == "FENCED") {
        return result.map(|_| ());
    }
    let mut tx = app.store.tx(lease.org_id).await?;
    crate::chat::lock(&mut tx, lease).await?;
    let value = match &result {
        Ok(conv) => json!({"conversation_id":conv}),
        Err(e) => json!({"error":e.to_string()}),
    };
    sqlx::query("UPDATE conversation_operations SET state=$3,result=$4 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(id)
        .bind(if result.is_ok() {
            "completed"
        } else {
            "failed"
        })
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
            state: if result.is_ok() {
                "completed"
            } else {
                "failed"
            }
            .into(),
            text: if result.is_ok() {
                "分叉对话已创建；文件基于当前工作区快照。".into()
            } else {
                "分叉失败，可查看命令状态后重试。".into()
            },
            ..Default::default()
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
async fn fork(app: &App, lease: &Lease, id: Uuid, payload: Value) -> anyhow::Result<Uuid> {
    let mut tx = app.store.tx(lease.org_id).await?;
    crate::chat::lock(&mut tx, lease).await?;
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
    let source=sqlx::query("SELECT creator_id,mode,title,run_config,project_id,model_credential_id FROM conversations WHERE org_id=$1 AND id=$2").bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    let work = source.get::<&str, _>("mode") == "work";
    let checkpoint = if work {
        Some(crate::checkpoint::create(app, lease, id).await?)
    } else {
        None
    };
    let mut tx = app.store.tx(lease.org_id).await?;
    crate::chat::lock(&mut tx, lease).await?;
    let mut seed = json!([]);
    let mut native = Value::Null;
    if let Some(checkpoint) = checkpoint {
        let manifest: Value =
            sqlx::query_scalar("SELECT manifest FROM checkpoints WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(checkpoint)
                .fetch_one(&mut *tx)
                .await?;
        native = json!({"checkpoint_id":checkpoint,"manifest":manifest,"last_turn_id":payload["turn_id"],"message_id":payload["message_id"]});
        sqlx::query("INSERT INTO restore_pins(id,org_id,checkpoint_id) VALUES($1,$2,$1) ON CONFLICT DO NOTHING").bind(checkpoint).bind(lease.org_id).execute(&mut *tx).await?;
    } else {
        let row=sqlx::query("SELECT s.step_no,s.request_messages,s.native_response,s.protocol FROM loop_steps s WHERE s.org_id=$1 AND s.run_id=$2 AND s.item_id=$3 AND s.status='completed'").bind(lease.org_id).bind(Uuid::parse_str(payload["run_id"].as_str().context("INVALID_FORK")?)?).bind(Uuid::parse_str(payload["item_id"].as_str().context("INVALID_FORK")?)?).fetch_optional(&mut *tx).await?.context("FORK_POINT_UNAVAILABLE")?;
        seed = row.get("request_messages");
        crate::chat::protocol::continue_with(
            row.get("protocol"),
            &mut seed,
            &row.get("native_response"),
            &[],
        )?;
    }
    sqlx::query("INSERT INTO conversations(id,org_id,creator_id,title,mode,run_config,project_id,model_credential_id,context_seed,native_fork,owner_epoch) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,1)").bind(id).bind(lease.org_id).bind(source.get::<Uuid,_>("creator_id")).bind(format!("{} · 分叉",source.get::<String,_>("title"))).bind(source.get::<String,_>("mode")).bind(source.get::<Value,_>("run_config")).bind(source.get::<Option<Uuid>,_>("project_id")).bind(source.get::<Option<Uuid>,_>("model_credential_id")).bind(seed).bind(if native.is_null(){None}else{Some(native)}).execute(&mut *tx).await?;
    let child = Lease {
        conversation_id: id,
        org_id: lease.org_id,
        owner_id: lease.owner_id,
        epoch: 1,
    };
    sqlx::query("INSERT INTO channels(id,org_id,kind,conversation_id,writer_epoch,owner_id,lease_until) VALUES($1,$2,'conv',$3,1,$4,now()+interval '30 seconds')").bind(child.channel()).bind(lease.org_id).bind(id).bind(lease.owner_id).execute(&mut *tx).await?;
    let items=sqlx::query("SELECT kind,state,text FROM items WHERE org_id=$1 AND conversation_id=$2 AND last_seq<=$3 AND state='completed' ORDER BY first_seq").bind(lease.org_id).bind(lease.conversation_id).bind(payload["seq"].as_i64().context("INVALID_FORK")?).fetch_all(&mut *tx).await?;
    for row in items {
        append(
            &mut tx,
            lease.org_id,
            &child.channel(),
            Some(&child),
            "",
            Event::ItemUpserted(Item {
                id: Uuid::now_v7().to_string(),
                revision: 1,
                kind: row.get("kind"),
                state: row.get("state"),
                text: row.get("text"),
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
        &format!(
            "user:{}:{}",
            lease.org_id,
            source.get::<Uuid, _>("creator_id")
        ),
        None,
        "",
        Event::ItemUpserted(Item {
            id: id.to_string(),
            revision: 1,
            kind: "conversation".into(),
            state: "active".into(),
            text: format!("{} · 分叉", source.get::<String, _>("title")),
            ..Default::default()
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}
