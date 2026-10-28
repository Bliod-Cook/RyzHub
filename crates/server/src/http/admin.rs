//! The instance admin API (`/admin/*`). Every request is a signed-in instance admin's; who is
//! one is set in the database, never here.
use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{delete, get},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::authenticate;
use crate::{
    app::App,
    error::{ApiResult, Code},
    store::{Auth, Tx},
};

pub fn routes() -> Router<App> {
    Router::new()
        .route("/admin/settings", get(settings))
        .route("/admin/plugin-blocks", get(blocks).post(block))
        .route("/admin/plugin-blocks/{id}", delete(unblock))
}

/// The caller, who must be an instance admin.
pub async fn admin(app: &App, headers: &HeaderMap) -> anyhow::Result<Auth> {
    let auth = authenticate(app, headers).await?;
    require!(auth.is_instance_admin, Code::Forbidden);
    Ok(auth)
}

/// Record an admin's action in their personal organization's audit log: `detail` names the
/// target and what changed. Leaves `tx` scoped to that organization.
pub async fn audit(
    tx: &mut Tx<'_>,
    admin: &Auth,
    action: &str,
    resource: &str,
    detail: Value,
) -> anyhow::Result<()> {
    let org: Uuid = sqlx::query_scalar(
        "SELECT o.id FROM list_user_orgs($1) l JOIN organizations o ON o.id=l.id
         WHERE o.kind='personal' LIMIT 1",
    )
    .bind(admin.user_id)
    .fetch_optional(&mut **tx)
    .await?
    .context("an instance admin without a personal organization")?;
    sqlx::query("SELECT set_config('app.org_id',$1,true)")
        .bind(org.to_string())
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO audit_logs(id,org_id,actor_id,actor_kind,action,resource,result,detail)
         VALUES($1,$2,$3,'instance_admin',$4,$5,'allowed',$6)",
    )
    .bind(Uuid::now_v7())
    .bind(org)
    .bind(admin.user_id)
    .bind(action)
    .bind(resource)
    .bind(detail)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// `GET /admin/settings`: the effective instance configuration.
async fn settings(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    admin(&app, &headers).await?;
    let config = &app.config;
    let isolation = match (config.sandbox.enabled, config.sandbox.runtime.as_str()) {
        (false, _) => "none",
        (true, "runsc") => "user_kernel",
        (true, _) => "container",
    };
    Ok(Json(json!({
        "config": config,
        "backend": if config.sandbox.enabled { "containerd" } else { "disabled" },
        "isolation_level": isolation,
        "capabilities": {
            "stream": "text/0",
            "approval": "native",
            "resume": "loop_steps",
            "chat": config.llm.enabled.then_some("anthropic"),
            "work": config.work.enabled.then_some("codex"),
            "terminal": config.work.enabled && config.sandbox.enabled,
        },
    })))
}

#[derive(Deserialize)]
struct BlockRequest {
    name: Option<String>,
    content_hash: Option<String>,
    #[serde(default)]
    reason: String,
}

/// `GET /admin/plugin-blocks`: plugins stopped for the whole instance.
async fn blocks(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    admin(&app, &headers).await?;
    let rows: Vec<Value> =
        sqlx::query_scalar("SELECT to_jsonb(b) FROM plugin_blocks b ORDER BY created_at DESC")
            .fetch_all(&app.store.pool)
            .await?;
    Ok(Json(json!({ "blocks": rows })))
}

/// `POST /admin/plugin-blocks`: stop a plugin everywhere, by name or content hash, at once.
async fn block(
    State(app): State<App>,
    headers: HeaderMap,
    Json(request): Json<BlockRequest>,
) -> ApiResult<Json<Value>> {
    let auth = admin(&app, &headers).await?;
    require!(
        request.name.is_some() || request.content_hash.is_some(),
        Code::InvalidInput
    );
    let id = Uuid::now_v7();
    let mut tx = app.store.pool.begin().await?;
    sqlx::query("INSERT INTO plugin_blocks(id,name,content_hash,reason) VALUES($1,$2,$3,$4)")
        .bind(id)
        .bind(&request.name)
        .bind(&request.content_hash)
        .bind(&request.reason)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &auth,
        "admin.plugin.block",
        &id.to_string(),
        json!({"name": request.name, "content_hash": request.content_hash}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "id": id })))
}

/// `DELETE /admin/plugin-blocks/{id}`.
async fn unblock(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let auth = admin(&app, &headers).await?;
    let mut tx = app.store.pool.begin().await?;
    sqlx::query("DELETE FROM plugin_blocks WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &auth,
        "admin.plugin.unblock",
        &id.to_string(),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "removed": id })))
}
