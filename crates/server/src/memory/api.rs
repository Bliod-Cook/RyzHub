use super::*;
use crate::{
    error::ApiResult,
    http::{authenticate, member_tx},
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
};
use std::collections::HashMap;

#[derive(Default, Deserialize)]
pub struct ListQuery {
    pub project: Option<Uuid>,
    pub q: Option<String>,
    #[serde(default)]
    pub sort: String,
}
pub async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    scope(&mut tx, org, ctx.user_id, q.project).await?;
    require!(
        q.q.as_ref().is_none_or(|q| q.len() <= 512)
            && matches!(q.sort.as_str(), "" | "priority" | "time"),
        Code::InvalidArguments
    );
    let items: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(m) FROM memory_items m WHERE org_id=$1 AND user_id=$2 AND status='active'
           AND project_id IS NOT DISTINCT FROM $3::uuid
           AND ($4::text IS NULL OR content &@~ pgroonga_query_escape($4))
         ORDER BY CASE WHEN $5='time' THEN updated_at END DESC,priority DESC,updated_at DESC,id
         LIMIT 1000",
    )
    .bind(org)
    .bind(ctx.user_id)
    .bind(q.project)
    .bind(q.q.filter(|s| !s.is_empty()))
    .bind(q.sort)
    .fetch_all(&mut *tx)
    .await?;
    let prefs = preferences(&mut tx, org, ctx.user_id).await?;
    let policy = effective_policy(&mut tx, &app.config().policy, org).await?;
    tx.commit().await?;
    let enabled = policy.allow_memory != Some(false);
    Ok(Json(
        json!({"items": items, "preferences": prefs, "policy_enabled": enabled}),
    ))
}
pub async fn put(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(input): Json<Save>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let user = auth.user_id;
    let mut tx = app.store.tx(org).await?;
    // Membership is checked once: with the source conversation's access, else on its own.
    match input.source_conversation_id {
        Some(conv) => {
            let a = access(&mut tx, &app.config(), org, conv, user).await?;
            require!(
                a.enabled && a.project == input.project_id,
                Code::PolicyDenied
            );
        }
        None => {
            context_in(&mut tx, user, org).await?;
        }
    }
    let policy = effective_policy(&mut tx, &app.config().policy, org).await?;
    require!(policy.allow_memory != Some(false), Code::PolicyDenied);
    let prefs = preferences(&mut tx, org, user).await?;
    require!(prefs["enabled"] == true, Code::PolicyDenied);
    let words = super::forbidden_words(&policy);
    let result = save(&mut tx, org, user, input, &words).await?;
    tx.commit().await?;
    Ok(Json(result))
}
pub async fn remove(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let project: Option<Uuid> = sqlx::query_scalar(
        "SELECT project_id FROM memory_items WHERE org_id=$1 AND user_id=$2 AND id=$3",
    )
    .bind(org)
    .bind(ctx.user_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .context(Code::Forbidden)?;
    scope(&mut tx, org, ctx.user_id, project).await?;
    let result = forget(&mut tx, org, ctx.user_id, id, project, None).await?;
    tx.commit().await?;
    Ok(Json(result))
}
#[derive(Deserialize)]
pub struct Preferences {
    pub enabled: bool,
    pub reference_history: bool,
}
pub async fn set_preferences(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(input): Json<Preferences>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let was_enabled = preferences(&mut tx, org, ctx.user_id).await?["enabled"] == true;
    sqlx::query(
        "INSERT INTO memory_preferences(org_id,user_id,enabled,reference_history,
           history_disabled_at)
         VALUES($1,$2,$3,$4,CASE WHEN $4 THEN NULL ELSE now() END)
         ON CONFLICT(org_id,user_id) DO UPDATE SET
           enabled=excluded.enabled,reference_history=excluded.reference_history,
           history_disabled_at=CASE WHEN excluded.reference_history THEN NULL
             ELSE coalesce(memory_preferences.history_disabled_at,now()) END",
    )
    .bind(org)
    .bind(ctx.user_id)
    .bind(input.enabled)
    .bind(input.reference_history)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE conversations SET memory_snapshot=NULL WHERE org_id=$1 AND creator_id=$2")
        .bind(org)
        .bind(ctx.user_id)
        .execute(&mut *tx)
        .await?;
    if input.enabled && !was_enabled {
        sqlx::query(
            "INSERT INTO search_jobs(org_id,source_id,source_kind,source_version,op)
             SELECT org_id,id,'memory',version,'upsert' FROM memory_items
             WHERE org_id=$1 AND user_id=$2 AND status='active'
             ON CONFLICT(org_id,source_id,source_version) DO UPDATE SET state='queued',attempts=0,
               available_at=now(),priority=0,claim_token=NULL,lease_until=NULL",
        )
        .bind(org)
        .bind(ctx.user_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Json(json!({"updated":true})))
}
/// Timeline events store references only. Resolve details with current permissions on expansion.
pub async fn timeline_context(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv, item)): Path<(Uuid, Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let user = authenticate(&app, &headers).await?.user_id;
    let mut tx = app.store.tx(org).await?;
    let access = super::access(&mut tx, &app.config(), org, conv, user).await?;
    let text: String = sqlx::query_scalar(
        "SELECT text FROM items WHERE org_id=$1 AND conversation_id=$2 AND id=$3
           AND kind='memory_context'",
    )
    .bind(org)
    .bind(conv)
    .bind(item)
    .fetch_optional(&mut *tx)
    .await?
    .context(Code::Forbidden)?;
    let mut entries = Vec::<Value>::new();
    if access.enabled {
        let references = serde_json::from_str::<Vec<Value>>(&text)?
            .into_iter()
            .map(|r| {
                let id = r["id"].as_str().context(Code::InvalidArguments)?;
                Ok((Uuid::parse_str(id)?, r["kind"] == "saved"))
            })
            .collect::<anyhow::Result<Vec<(Uuid, bool)>>>()?;
        let ids = |saved: bool| -> Vec<Uuid> {
            references
                .iter()
                .filter(|(_, s)| *s == saved)
                .map(|(id, _)| *id)
                .collect()
        };
        let saved: HashMap<Uuid, Value> = sqlx::query_as(
            "SELECT id,jsonb_build_object('id',id,'content',content) FROM memory_items
             WHERE org_id=$1 AND user_id=$2 AND id=ANY($3) AND status='active'
               AND project_id IS NOT DISTINCT FROM $4::uuid",
        )
        .bind(org)
        .bind(user)
        .bind(ids(true))
        .bind(access.project)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
        let visible = if access.history {
            visible_history(&mut tx, org, user, access.project, &ids(false)).await?
        } else {
            vec![]
        };
        let recent: HashMap<Uuid, Value> = sqlx::query_as(
            "SELECT conversation_id,jsonb_build_object('id',conversation_id,'title',title,
               'points',user_points,'source','/orgs/'||org_id||'/conversations/'||conversation_id)
             FROM conversation_digests WHERE org_id=$1 AND conversation_id=ANY($2) AND user_id=$3",
        )
        .bind(org)
        .bind(&visible)
        .bind(user)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
        entries = references
            .iter()
            .filter_map(|(id, saved_kind)| {
                let found = if *saved_kind { &saved } else { &recent };
                found.get(id).cloned()
            })
            .collect();
    }
    tx.commit().await?;
    Ok(Json(json!({"items":entries})))
}

pub const TOOLS: [&str; 4] = [
    "conversation_search",
    "memory_search",
    "memory_save",
    "memory_forget",
];
pub fn definitions() -> Vec<Value> {
    let query =
        json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]});
    vec![
        json!({"name":"conversation_search","description":"Search your permitted historical chats; cite the source links in your answer. Project-only tasks stay in their project.","input_schema":query}),
        json!({"name":"memory_search","description":"Search explicit memories in the current memory scope.","input_schema":query}),
        json!({"name":"memory_save","description":"Save only facts the user explicitly asked you to remember. Use replace to correct an existing memory. Never store secrets. Writes apply immediately.","input_schema":{"type":"object","properties":{"content":{"type":"string"},"replace":{"type":"string"},"priority":{"type":"integer"}},"required":["content"]}}),
        json!({"name":"memory_forget","description":"Forget an explicit memory when asked by the user.","input_schema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}}),
    ]
}
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    tx: &mut Tx<'_>,
    app: &App,
    org: Uuid,
    run: Uuid,
    name: &str,
    args: Value,
    key: String,
    vector: Option<String>,
) -> anyhow::Result<Value> {
    let a = run_access(tx, app, org, run).await?;
    ensure!(a.enabled, Code::PolicyDenied);
    let result = match name {
        "conversation_search" | "memory_search" => {
            ensure!(
                name != "conversation_search" || a.history,
                Code::PolicyDenied
            );
            let (query, mode) = search_args(app, &args)?;
            let q = crate::search::SearchQuery {
                q: query.into(),
                mode: mode.into(),
                ..Default::default()
            };
            let kind = if name == "memory_search" {
                "memory"
            } else {
                "conversations"
            };
            let model = &app.config().search.embedding_model;
            let boundary = a.boundary();
            let matches =
                crate::search::search(tx, org, a.user, &q, boundary, kind, vector, model).await?;
            return Ok(json!({"matches":matches}));
        }
        "memory_save" => {
            let mut input: Save =
                serde_json::from_value(args).context(Code::InvalidToolArguments)?;
            input.project_id = a.project;
            input.source_conversation_id = Some(a.conversation);
            input.request_key = Some(key);
            let policy = crate::store::effective_policy(tx, &app.config().policy, org).await?;
            save(tx, org, a.user, input, &super::forbidden_words(&policy)).await?
        }
        "memory_forget" => {
            let id = Uuid::parse_str(args["id"].as_str().context(Code::InvalidToolArguments)?)
                .context(Code::InvalidToolArguments)?;
            forget(tx, org, a.user, id, a.project, Some(key)).await?
        }
        _ => anyhow::bail!(Code::ToolDenied),
    };
    // The write's notice goes to the conversation that made it.
    let event = result["event_id"]
        .as_str()
        .context(Code::InvalidArguments)?;
    sqlx::query("UPDATE memory_events SET origin_conversation_id=$3 WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(Uuid::parse_str(event)?)
        .bind(a.conversation)
        .execute(&mut **tx)
        .await?;
    Ok(result)
}

/// A search tool's query and mode (hybrid by default when embeddings are configured).
fn search_args<'a>(app: &App, args: &'a Value) -> anyhow::Result<(&'a str, &'a str)> {
    let query = args["query"].as_str().context(Code::InvalidToolArguments)?;
    let mode = args["mode"]
        .as_str()
        .unwrap_or(if app.config().search.embeddings() {
            "hybrid"
        } else {
            "fulltext"
        });
    ensure!(
        !mode.is_empty() && crate::search::valid(query, mode),
        Code::InvalidToolArguments
    );
    Ok((query, mode))
}
/// Embeddings are prepared without keeping an Owner lease or database transaction locked.
pub async fn prepare_search(
    app: &App,
    org: Uuid,
    run: Uuid,
    name: &str,
    args: &Value,
) -> anyhow::Result<Option<String>> {
    if !matches!(name, "conversation_search" | "memory_search") {
        return Ok(None);
    }
    let (query, mode) = search_args(app, args)?;
    if mode == "fulltext" {
        return Ok(None);
    }
    let mut tx = app.store.tx(org).await?;
    let a = run_access(&mut tx, app, org, run).await?;
    ensure!(
        a.enabled && (name != "conversation_search" || a.history),
        Code::PolicyDenied
    );
    tx.commit().await?;
    crate::search::query_vector(app, org, a.user, query, mode).await
}
