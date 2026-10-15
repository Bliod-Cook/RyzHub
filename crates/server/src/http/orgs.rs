//! Organization-scoped REST resources: conversations, history, members, policy, models, audit.
use anyhow::Context;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use ryzhub_domain::{Action, Principal, Resource, Role, authorize};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::App,
    error::{ApiResult, Code},
    run_config,
    store::{Conversation, audit, effective_policy},
};

#[derive(Deserialize)]
pub struct CreateConversation {
    title: String,
    #[serde(default)]
    temporary: bool,
    /// Empty: the project's default mode, else chat.
    #[serde(default)]
    mode: String,
    #[serde(default)]
    project_id: Option<Uuid>,
    #[serde(default = "empty_object")]
    run_config: Value,
}

fn empty_object() -> Value {
    json!({})
}

/// `POST /orgs/{org}/conversations`
pub async fn create_conversation(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(input): Json<CreateConversation>,
) -> ApiResult<Json<Conversation>> {
    let (ctx, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    let mut settings = input.run_config;
    if input.temporary {
        settings["temporary"] = json!(true);
    }
    let conversation = crate::store::create_in(
        &mut tx,
        &ctx,
        Uuid::now_v7(),
        &input.title,
        &input.mode,
        settings,
        input.project_id,
        Some(&app.config),
    )
    .await?
    .context(Code::ConversationIdTaken)?;
    tx.commit().await?;
    Ok(Json(conversation))
}

/// `GET /orgs/{org}/conversations`
pub async fn list_conversations(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<Vec<Conversation>>> {
    let (ctx, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    let list = crate::store::list_conversations(&mut tx, &ctx).await?;
    tx.commit().await?;
    Ok(Json(list))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    before_seq: Option<i64>,
}

/// `GET /orgs/{org}/conversations/{conv}/items?before_seq=`: one page, newest first.
pub async fn history(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Query(query): Query<HistoryQuery>,
) -> ApiResult<Json<Value>> {
    let ctx = crate::http::member(&app, &headers, org).await?;
    let before = query.before_seq.unwrap_or(i64::MAX);
    let (items, more) = app.store.history(&ctx, conv, before).await?;
    // `has_more` lets clients stop paging without guessing from the page size.
    Ok(Json(json!({"items":items,"has_more":more})))
}

/// `GET /orgs/{org}/policy`: the organization policy narrowed by the instance.
pub async fn policy(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<ryzhub_domain::Policy>> {
    let (_, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    let policy = effective_policy(&mut tx, &app.config.policy, org).await?;
    tx.commit().await?;
    Ok(Json(policy))
}

/// `GET /orgs/{org}/models`: models a member may start with, grouped as clients pick them:
/// Chat by model, Work by harness and then model. The wire protocol follows the provider.
pub async fn models(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let (_, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    let policy = effective_policy(&mut tx, &app.config.policy, org).await?;
    tx.commit().await?;
    let config = &app.config;
    let catalog = run_config::catalog(config);
    let list = |harness: &str| -> Vec<Value> {
        let default = match (harness, run_config::acp(config, harness)) {
            (_, Some(profile)) => profile.model.as_str(),
            ("codex", _) => config.work.model.as_str(),
            ("claude", _) => config.work.claude_model.as_str(),
            _ => config.llm.model.as_str(),
        };
        let allowed = policy
            .harnesses
            .as_ref()
            .is_none_or(|h| h.contains(harness));
        let mut models: Vec<&run_config::ModelEntry> = catalog
            .iter()
            .filter(|m| allowed && m.harnesses.iter().any(|h| h == harness))
            .filter(|m| policy.models.as_ref().is_none_or(|p| p.contains(&m.id)))
            .collect();
        // Stable sort: the default leads, so pickers open on it.
        models.sort_by_key(|m| m.id != default);
        models
            .into_iter()
            .map(|m| {
                let provider = run_config::provider(&m.protocol);
                let levels = run_config::reasoning_levels(harness, &m.protocol);
                json!({"id":m.id,"provider":provider,"default":m.id==default,"reasoning_levels":levels})
            })
            .collect()
    };
    let harnesses = if config.work.enabled {
        run_config::work_harnesses(config)
    } else {
        vec![]
    };
    let work: Vec<Value> = harnesses
        .into_iter()
        .map(|harness| (harness, list(harness)))
        .filter(|(_, models)| !models.is_empty())
        .map(|(harness, models)| {
            json!({"harness":harness,"label":run_config::harness_label(config, harness),"models":models})
        })
        .collect();
    Ok(Json(json!({"chat":list("chat"),"work":work})))
}

/// `GET /orgs/{org}/audit`: owners and admins only.
// TODO(org): no client reads it yet.
pub async fn audit_log(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<Vec<Value>>> {
    let ctx = crate::http::member(&app, &headers, org).await?;
    authorize(
        &Principal::User(ctx),
        Action::ViewAudit,
        &Resource {
            org_id: org,
            id: org,
            owner_user_id: None,
            target_role: None,
        },
    )
    .map_err(|_| Code::Forbidden)?;
    let mut tx = app.store.tx(org).await?;
    let rows = sqlx::query_scalar(
        "SELECT to_jsonb(a) FROM (SELECT id,actor_id,action,resource,result,created_at FROM audit_logs
         WHERE org_id=$1 ORDER BY created_at DESC LIMIT 100) a",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(rows))
}

/// `DELETE /orgs/{org}/members/{user}`: removes the membership and revokes the member's
/// gateway keys in this organization. Subscriptions and model streams in this organization end
/// through the revocation notice; the member's sessions stay valid for their other
/// organizations. Their conversations remain in the organization.
// TODO(org): no client calls it; only registration adds a member (the owner).
pub async fn remove_member(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, user)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let ctx = crate::http::member(&app, &headers, org).await?;
    let target = app.store.context(user, org).await?;
    authorize(
        &Principal::User(ctx.clone()),
        Action::ManageMember,
        &Resource {
            org_id: org,
            id: user,
            owner_user_id: None,
            target_role: Some(target.role),
        },
    )
    .map_err(|_| Code::Forbidden)?;
    require!(target.role != Role::Owner, Code::Forbidden);
    let mut tx = app.store.tx(org).await?;
    sqlx::query("DELETE FROM memberships WHERE org_id=$1 AND user_id=$2")
        .bind(org)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE virtual_keys SET revoked_at=now() WHERE org_id=$1 AND user_id=$2 AND revoked_at IS NULL")
        .bind(org)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &ctx, "member.remove", &user.to_string(), "allowed").await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
