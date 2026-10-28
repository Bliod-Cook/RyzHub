//! The instance admin API (`/admin/*`). Every request is a signed-in instance admin's; who is
//! one is set in the database, never here.
use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::HeaderMap,
    routing::{delete, get},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

mod components;
mod config;
mod users;

use super::authenticate;
use crate::{
    app::App,
    error::{ApiResult, Code},
    store::{Auth, Tx},
};

pub fn routes() -> Router<App> {
    users::routes()
        .merge(config::routes())
        .merge(components::routes())
        .route("/admin/stats", get(stats))
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
    let org: Uuid = sqlx::query_scalar::<_, Option<Uuid>>("SELECT personal_org($1)")
        .bind(admin.user_id)
        .fetch_one(&mut **tx)
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

#[derive(Deserialize)]
struct StatsQuery {
    days: i32,
    /// The viewer's offset from UTC in minutes, east positive: days are counted in it.
    tz_offset: i32,
}

/// `GET /admin/stats?days=7|30|90&tz_offset=`: every overview figure, from data the instance
/// keeps anyway.
async fn stats(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<StatsQuery>,
) -> ApiResult<Json<Value>> {
    admin(&app, &headers).await?;
    require!(
        matches!(query.days, 7 | 30 | 90) && (-14 * 60..=14 * 60).contains(&query.tz_offset),
        Code::InvalidInput
    );
    let mut stats: Value = sqlx::query_scalar("SELECT instance_stats($1,make_interval(mins=>$2))")
        .bind(query.days)
        .bind(query.tz_offset)
        .fetch_one(&app.store.pool)
        .await?;
    let retention = &app.config().retention;
    stats["retention"] = json!({
        "usage_days": retention.usage_days,
        "audit_days": retention.audit_days,
    });
    Ok(Json(stats))
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
