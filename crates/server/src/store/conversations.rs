//! Conversation creation and listing.
use anyhow::{Context, ensure};
use ryzhub_domain::{Action, OrgContext, Principal, Resource, authorize};
use ryzhub_proto::event_envelope::Event;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    Conversation, Store, Tx, append, audit, context_in, conversation_item, effective_policy,
};
use crate::{config::Config, error::Code, run_config::RunConfig};

impl Store {
    pub async fn create_conversation(
        &self,
        ctx: &OrgContext,
        title: &str,
        mode: &str,
    ) -> anyhow::Result<Conversation> {
        self.create_configured(ctx, title, mode, json!({}), None, None)
            .await
    }

    /// Create a conversation, optionally inside a project whose defaults fill in the settings.
    /// With `config`, the run configuration is resolved and validated against policy now.
    pub async fn create_configured(
        &self,
        ctx: &OrgContext,
        title: &str,
        mode: &str,
        settings: Value,
        project: Option<Uuid>,
        config: Option<&Config>,
    ) -> anyhow::Result<Conversation> {
        let mut tx = self.tx(ctx.org_id).await?;
        let ctx = context_in(&mut tx, ctx.user_id, ctx.org_id).await?;
        let conversation = create_in(
            &mut tx,
            &ctx,
            Uuid::now_v7(),
            title,
            mode,
            settings,
            project,
            config,
        )
        .await?
        .context(Code::ConversationIdTaken)?;
        tx.commit().await?;
        Ok(conversation)
    }
}

/// The caller's visible conversations, newest first.
pub async fn list_conversations(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
) -> anyhow::Result<Vec<Conversation>> {
    // Pins are kept within the limit, but the list stays newest first: clients order pins
    // themselves, and an unpinned conversation returns to its place among the others.
    Ok(sqlx::query_as(concat!(
        "SELECT * FROM (SELECT ",
        conversation_columns!(),
        ",created_at FROM conversations
          WHERE org_id=$1 AND creator_id=$2 AND state IN ('active','archived')
          ORDER BY pinned_at DESC NULLS LAST, created_at DESC LIMIT 200) c
         ORDER BY created_at DESC"
    ))
    .bind(ctx.org_id)
    .bind(ctx.user_id)
    .fetch_all(&mut **tx)
    .await?)
}

/// Create conversation `id` in `tx` for `ctx`, a membership resolved in `tx`: None when a
/// conversation already has that id. See [`Store::create_configured`].
#[allow(clippy::too_many_arguments)]
pub async fn create_in(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
    id: Uuid,
    title: &str,
    mode: &str,
    settings: Value,
    project: Option<Uuid>,
    config: Option<&Config>,
) -> anyhow::Result<Option<Conversation>> {
    ensure!(
        matches!(mode, "" | "chat" | "work") && !title.trim().is_empty() && title.len() <= 256,
        Code::InvalidConversation
    );
    authorize(
        &Principal::User(ctx.clone()),
        Action::UseConversation,
        &Resource {
            org_id: ctx.org_id,
            id: ctx.org_id,
            owner_user_id: Some(ctx.user_id),
            target_role: None,
        },
    )
    .map_err(|_| Code::Forbidden)?;
    let (mode, mut settings) = match project {
        Some(project) => {
            if let Some(config) = config {
                let policy = effective_policy(tx, &config.policy, ctx.org_id).await?;
                ensure!(policy.allow_projects != Some(false), Code::PolicyDenied);
            }
            apply_project_defaults(tx, ctx, project, mode, settings).await?
        }
        None => (mode.to_owned(), settings),
    };
    let mode = if mode.is_empty() { "chat".into() } else { mode };
    ensure!(matches!(mode.as_str(), "chat" | "work"), Code::InvalidMode);
    let temporary = settings
        .as_object_mut()
        .and_then(|s| s.remove("temporary"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(config) = config {
        let resolved = RunConfig::resolve(settings, &mode, config)?;
        resolved.validate(tx, ctx.org_id, config).await?;
        settings = serde_json::to_value(resolved)?;
    }
    // An id taken in any organization is taken.
    let conversation: Option<Conversation> = sqlx::query_as(concat!(
        "INSERT INTO conversations(id,org_id,creator_id,title,mode,run_config,project_id,temporary,expires_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,CASE WHEN $8 THEN now()+interval '30 days' END)
         ON CONFLICT(id) DO NOTHING RETURNING ",
        conversation_columns!()
    ))
    .bind(id)
    .bind(ctx.org_id)
    .bind(ctx.user_id)
    .bind(title)
    .bind(&mode)
    .bind(settings)
    .bind(project)
    .bind(temporary)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(conversation) = conversation else {
        return Ok(None);
    };
    sqlx::query(
        "INSERT INTO channels(id,org_id,kind,conversation_id,writer_epoch) VALUES($1,$2,'conv',$3,0)",
    )
    .bind(format!("conv:{id}"))
    .bind(ctx.org_id)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    append(
        tx,
        ctx.org_id,
        &format!("user:{}:{}", ctx.org_id, ctx.user_id),
        None,
        "",
        Event::ItemUpserted(conversation_item(&conversation, 1)),
    )
    .await?;
    audit(tx, ctx, "conversation.create", &id.to_string(), "allowed").await?;
    Ok(Some(conversation))
}

/// Project defaults describe one path (mode, harness, model). A request for a different mode,
/// harness or model replaces the parts of that path it implies instead of mixing them.
async fn apply_project_defaults(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
    project: Uuid,
    mode: &str,
    settings: Value,
) -> anyhow::Result<(String, Value)> {
    let defaults: Value = sqlx::query_scalar(
        "SELECT defaults FROM projects WHERE org_id=$1 AND id=$2 AND creator_id=$3 AND state='active' FOR UPDATE",
    )
    .bind(ctx.org_id)
    .bind(project)
    .bind(ctx.user_id)
    .fetch_optional(&mut **tx)
    .await?
    .context(Code::Forbidden)?;
    let mut merged: Map<String, Value> =
        defaults.as_object().context(Code::InvalidDefaults)?.clone();
    let default_mode = merged
        .remove("mode")
        .and_then(|m| m.as_str().map(str::to_owned))
        .unwrap_or_else(|| "chat".into());
    let request = settings.as_object().context(Code::InvalidConfig)?;
    let other_mode = !mode.is_empty() && default_mode != mode;
    if other_mode || request.contains_key("harness") {
        for key in ["harness", "model", "protocol"] {
            merged.remove(key);
        }
    }
    if request.contains_key("model") {
        merged.remove("protocol");
    }
    merged.extend(request.clone());
    let mode = if mode.is_empty() {
        default_mode
    } else {
        mode.to_owned()
    };
    Ok((mode, Value::Object(merged)))
}
