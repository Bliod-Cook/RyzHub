use super::*;
use crate::{error::ApiResult, http::member_tx};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};

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
    let items:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(m) FROM memory_items m WHERE org_id=$1 AND user_id=$2 AND status='active'
        AND project_id IS NOT DISTINCT FROM $3::uuid AND ($4::text IS NULL OR content &@~ pgroonga_query_escape($4))
        ORDER BY CASE WHEN $5='time' THEN updated_at END DESC,priority DESC,updated_at DESC,id LIMIT 1000")
        .bind(org).bind(ctx.user_id).bind(q.project).bind(q.q.filter(|s|!s.is_empty())).bind(q.sort).fetch_all(&mut *tx).await?;
    let prefs = preferences(&mut tx, org, ctx.user_id).await?;
    let policy = effective_policy(&mut tx, &app.config.policy, org).await?;
    tx.commit().await?;
    Ok(Json(
        json!({"items":items,"preferences":prefs,"policy_enabled":policy.allow_memory!=Some(false)}),
    ))
}
pub async fn put(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(input): Json<Save>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let policy = effective_policy(&mut tx, &app.config.policy, org).await?;
    require!(policy.allow_memory != Some(false), Code::PolicyDenied);
    let prefs = preferences(&mut tx, org, ctx.user_id).await?;
    require!(prefs["enabled"] == true, Code::PolicyDenied);
    if let Some(conv) = input.source_conversation_id {
        let a = access(&mut tx, &app.config, org, conv, ctx.user_id).await?;
        require!(
            a.enabled && a.project == input.project_id,
            Code::PolicyDenied
        );
    }
    let result = save(
        &mut tx,
        org,
        ctx.user_id,
        input,
        &super::forbidden_words(&policy),
    )
    .await?;
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
pub async fn revert(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let result = undo(&mut tx, org, ctx.user_id, id).await?;
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
    sqlx::query("INSERT INTO memory_preferences(org_id,user_id,enabled,reference_history,history_disabled_at)
        VALUES($1,$2,$3,$4,CASE WHEN $4 THEN NULL ELSE now() END) ON CONFLICT(org_id,user_id) DO UPDATE SET
        enabled=excluded.enabled,reference_history=excluded.reference_history,
        history_disabled_at=CASE WHEN excluded.reference_history THEN NULL ELSE coalesce(memory_preferences.history_disabled_at,now()) END")
        .bind(org).bind(ctx.user_id).bind(input.enabled).bind(input.reference_history).execute(&mut *tx).await?;
    sqlx::query("UPDATE conversations SET memory_snapshot=NULL WHERE org_id=$1 AND creator_id=$2")
        .bind(org)
        .bind(ctx.user_id)
        .execute(&mut *tx)
        .await?;
    if input.enabled && !was_enabled {
        sqlx::query("INSERT INTO search_jobs(org_id,source_id,source_kind,source_version,op) SELECT org_id,id,'memory',version,'upsert' FROM memory_items WHERE org_id=$1 AND user_id=$2 AND status='active' ON CONFLICT(org_id,source_id,source_version) DO UPDATE SET state='queued',attempts=0,available_at=now(),priority=0,claim_token=NULL,lease_until=NULL")
            .bind(org).bind(ctx.user_id).execute(&mut *tx).await?;
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
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let access = super::access(&mut tx, &app.config, org, conv, ctx.user_id).await?;
    let text:String=sqlx::query_scalar("SELECT text FROM items WHERE org_id=$1 AND conversation_id=$2 AND id=$3 AND kind='memory_context'")
        .bind(org).bind(conv).bind(item).fetch_optional(&mut *tx).await?.context(Code::Forbidden)?;
    let mut entries = Vec::<Value>::new();
    if access.enabled {
        for reference in serde_json::from_str::<Vec<Value>>(&text)? {
            let id = Uuid::parse_str(reference["id"].as_str().context(Code::InvalidArguments)?)?;
            if reference["kind"] == "saved" {
                let entry:Option<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'content',content) FROM memory_items WHERE org_id=$1 AND user_id=$2 AND id=$3 AND status='active' AND project_id IS NOT DISTINCT FROM $4::uuid")
                    .bind(org).bind(ctx.user_id).bind(id).bind(access.project).fetch_optional(&mut *tx).await?;
                entries.extend(entry);
            } else if reference["kind"] == "recent"
                && access.history
                && super::access(&mut tx, &app.config, org, id, ctx.user_id)
                    .await
                    .is_ok_and(|a| a.enabled && a.project == access.project)
            {
                let entry:Option<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',conversation_id,'title',title,'points',user_points,'source','/orgs/'||org_id||'/conversations/'||conversation_id) FROM conversation_digests WHERE org_id=$1 AND conversation_id=$2 AND user_id=$3")
                    .bind(org).bind(id).bind(ctx.user_id).fetch_optional(&mut *tx).await?;
                entries.extend(entry);
            }
        }
    }
    tx.commit().await?;
    Ok(Json(json!({"items":entries})))
}
#[derive(Default, Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    pub delete_memories: bool,
}
pub async fn delete_conversation(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult<StatusCode> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    sqlx::query("SELECT id FROM channels WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE")
        .bind(org)
        .bind(conv)
        .fetch_optional(&mut *tx)
        .await?
        .context(Code::Forbidden)?;
    crate::store::conversation_in(&mut tx, &ctx, conv).await?;
    crate::projects::idle(&mut tx, org, conv).await?;
    let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('queued','running'))")
        .bind(org).bind(conv).fetch_one(&mut *tx).await?;
    require!(!busy, Code::WorkspaceBusy);
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
        sqlx::query("UPDATE memory_items SET status='deleted',invalid_at=now(),version=version+1 WHERE org_id=$1 AND user_id=$2 AND source_conversation_id=$3 AND status='active'")
            .bind(org).bind(ctx.user_id).bind(conv).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE virtual_keys SET revoked_at=now() WHERE org_id=$1 AND conversation_id=$2 AND revoked_at IS NULL")
        .bind(org).bind(conv).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub const TOOLS: [&str; 4] = [
    "conversation_search",
    "memory_search",
    "memory_save",
    "memory_forget",
];
pub fn definitions() -> Vec<Value> {
    vec![
        json!({"name":"conversation_search","description":"Search your permitted historical chats; cite the source links in your answer. Project-only tasks stay in their project.","input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}),
        json!({"name":"memory_search","description":"Search explicit memories in the current memory scope.","input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}),
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
    match name {
        "conversation_search" | "memory_search" => {
            ensure!(
                name != "conversation_search" || a.history,
                Code::PolicyDenied
            );
            let query = args["query"].as_str().context(Code::InvalidToolArguments)?;
            ensure!(
                !query.trim().is_empty() && query.len() <= 512,
                Code::InvalidToolArguments
            );
            let q = crate::search::SearchQuery {
                q: query.into(),
                mode: search_mode(app, &args)?.to_owned(),
                ..Default::default()
            };
            use crate::search::SearchBackend;
            let matches = crate::search::PostgresSearch
                .search(
                    tx,
                    org,
                    a.user,
                    &q,
                    a.boundary(),
                    if name == "memory_search" {
                        "memory"
                    } else {
                        "conversations"
                    },
                    vector,
                    &app.config.search.embedding_model,
                )
                .await?;
            Ok(json!({"matches":matches}))
        }
        "memory_save" => {
            let mut input: Save =
                serde_json::from_value(args).context(Code::InvalidToolArguments)?;
            input.project_id = a.project;
            input.source_conversation_id = Some(a.conversation);
            input.request_key = Some(key);
            let policy = crate::store::effective_policy(tx, &app.config.policy, org).await?;
            let result = save(tx, org, a.user, input, &super::forbidden_words(&policy)).await?;
            sqlx::query(
                "UPDATE memory_events SET origin_conversation_id=$3 WHERE org_id=$1 AND id=$2",
            )
            .bind(org)
            .bind(Uuid::parse_str(
                result["event_id"]
                    .as_str()
                    .context(Code::InvalidArguments)?,
            )?)
            .bind(a.conversation)
            .execute(&mut **tx)
            .await?;
            Ok(result)
        }
        "memory_forget" => {
            let id = Uuid::parse_str(args["id"].as_str().context(Code::InvalidToolArguments)?)
                .context(Code::InvalidToolArguments)?;
            let result = forget(tx, org, a.user, id, a.project, Some(key)).await?;
            sqlx::query(
                "UPDATE memory_events SET origin_conversation_id=$3 WHERE org_id=$1 AND id=$2",
            )
            .bind(org)
            .bind(Uuid::parse_str(
                result["event_id"]
                    .as_str()
                    .context(Code::InvalidArguments)?,
            )?)
            .bind(a.conversation)
            .execute(&mut **tx)
            .await?;
            Ok(result)
        }
        _ => anyhow::bail!(Code::ToolDenied),
    }
}

fn search_mode<'a>(app: &'a App, args: &'a Value) -> anyhow::Result<&'a str> {
    let mode = args["mode"]
        .as_str()
        .unwrap_or(if app.config.search.embeddings() {
            "hybrid"
        } else {
            "fulltext"
        });
    ensure!(
        matches!(mode, "fulltext" | "hybrid" | "vector"),
        Code::InvalidToolArguments
    );
    Ok(mode)
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
    let mode = search_mode(app, args)?;
    let query = args["query"].as_str().context(Code::InvalidToolArguments)?;
    ensure!(
        !query.trim().is_empty() && query.len() <= 512,
        Code::InvalidToolArguments
    );
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
    let vectors = crate::search::embedding::embed(app, org, a.user, vec![query.into()]).await;
    match vectors {
        Ok(vectors) => Ok(Some(crate::search::embedding::vector_literal(&vectors[0]))),
        Err(error) if mode == "hybrid" => {
            tracing::warn!(error=%error, "memory search using fulltext while embedding is unavailable");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
