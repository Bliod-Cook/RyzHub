//! `/admin/users`: every account of the instance, disabled, enabled, deleted and restored.
use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{admin, audit};
use crate::{
    accounts::{self, RECOVERABLE_DAYS},
    app::App,
    error::{ApiResult, Code},
    store::{Auth, Tx},
};

const PAGE: i64 = 50;

pub fn routes() -> Router<App> {
    Router::new()
        .route("/admin/users", get(list))
        .route("/admin/users/{id}", get(show).patch(change).delete(delete))
        .route("/admin/users/{id}/restore", post(restore))
}

/// An account as the panel lists it, from `users u`.
const ACCOUNT: &str = "jsonb_build_object(
    'id',u.id,'email',u.email,'name',u.display_name,'status',u.status,
    'is_instance_admin',u.is_instance_admin,
    'created_at_ms',(extract(epoch FROM uuid_extract_timestamp(u.id))*1000)::bigint,
    'password',u.password_hash LIKE '$%',
    'providers',coalesce((SELECT jsonb_agg(DISTINCT i.provider) FROM user_identities i
        WHERE i.user_id=u.id),'[]'),
    'last_sign_in_ms',(SELECT (extract(epoch FROM max(r.created_at))*1000)::bigint
        FROM auth_refresh r WHERE r.user_id=u.id),
    'delete_after_ms',(extract(epoch FROM u.delete_after)*1000)::bigint,
    'purging',u.status='deleted'
        AND (SELECT o.state FROM organizations o WHERE o.id=personal_org(u.id))='deleting')";

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    q: String,
    status: Option<String>,
    before: Option<Uuid>,
}

/// `GET /admin/users?q=&status=&before=`: newest first, a page at a time; `q` matches email
/// or name. Deleted accounts only when asked for.
async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    admin(&app, &headers).await?;
    require!(query.q.len() <= 254, Code::InvalidInput);
    let mut users: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {ACCOUNT} FROM users u
         WHERE ($1='' OR strpos(lower(u.email),lower($1))>0 OR strpos(lower(u.display_name),lower($1))>0)
           AND coalesce(u.status=$2,u.status<>'deleted') AND ($3::uuid IS NULL OR u.id<$3)
         ORDER BY u.id DESC LIMIT $4"
    )))
    .bind(&query.q)
    .bind(&query.status)
    .bind(query.before)
    .bind(PAGE + 1)
    .fetch_all(&app.store.pool)
    .await?;
    let next = (users.len() as i64 > PAGE).then(|| {
        users.truncate(PAGE as usize);
        users.last().map(|user| user["id"].clone())
    });
    Ok(Json(json!({"users": users, "next": next.flatten()})))
}

/// `GET /admin/users/{id}`: the account, how it signs in and what it has.
async fn show(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    admin(&app, &headers).await?;
    let mut tx = app.store.pool.begin().await?;
    let mut user = account(&mut tx, id).await?;
    user["identities"] = sqlx::query_scalar(
        "SELECT coalesce(jsonb_agg(jsonb_build_object('provider',provider,'subject',subject,
           'email',email,'created_at_ms',(extract(epoch FROM created_at)*1000)::bigint)
           ORDER BY created_at),'[]')
         FROM user_identities WHERE user_id=$1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    user["devices"] = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM auth_refresh WHERE user_id=$1 AND revoked_at IS NULL AND expires_at>now()",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?
    .into();
    // What the account has in its personal organization, the only kind there is.
    let org: Option<Uuid> = sqlx::query_scalar("SELECT personal_org($1)")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let (conversations, input, output): (i64, i64, i64) = match org {
        Some(org) => {
            sqlx::query("SELECT set_config('app.org_id',$1,true)")
                .bind(org.to_string())
                .execute(&mut *tx)
                .await?;
            sqlx::query_as(
                "SELECT (SELECT count(*) FROM conversations WHERE org_id=$1 AND creator_id=$2
                           AND state IN ('active','archived')),
                        coalesce(sum(input_tokens),0)::bigint,coalesce(sum(output_tokens),0)::bigint
                 FROM usage_records WHERE org_id=$1 AND user_id=$2
                   AND created_at>now()-interval '30 days'",
            )
            .bind(org)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?
        }
        None => (0, 0, 0),
    };
    tx.commit().await?;
    user["conversations"] = conversations.into();
    user["tokens_30d"] = json!({"input": input, "output": output});
    Ok(Json(user))
}

async fn account(tx: &mut Tx<'_>, id: Uuid) -> anyhow::Result<Value> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {ACCOUNT} FROM users u WHERE u.id=$1"
    )))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .context(Code::UserNotFound)
}

/// Lock the instance's active admins, then `target`, and return its status. The caller must
/// still be one of those admins and not the target: two admins acting on each other take the
/// same locks first, so the second finds itself no longer an admin, and an admin always
/// remains.
async fn lock(tx: &mut Tx<'_>, admin: &Auth, target: Uuid) -> anyhow::Result<String> {
    require!(target != admin.user_id, Code::CannotChangeSelf);
    let admins: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM users WHERE is_instance_admin AND status='active' ORDER BY id FOR UPDATE",
    )
    .fetch_all(&mut **tx)
    .await?;
    require!(admins.contains(&admin.user_id), Code::Forbidden);
    sqlx::query_scalar("SELECT status FROM users WHERE id=$1 FOR UPDATE")
        .bind(target)
        .fetch_optional(&mut **tx)
        .await?
        .context(Code::UserNotFound)
}

/// Set `target`'s status, as admin, and audit it as `action`.
async fn transition(
    app: &App,
    admin: &Auth,
    target: Uuid,
    from: &[&str],
    to: &str,
    action: &str,
) -> ApiResult<Json<Value>> {
    let mut tx = app.store.pool.begin().await?;
    let status = lock(&mut tx, admin, target).await?;
    require!(from.contains(&status.as_str()), Code::InvalidInput);
    if status == "active" {
        accounts::revoke(&mut tx, target).await?;
    }
    sqlx::query(
        "UPDATE users SET status=$2,
           delete_after=CASE WHEN $2='deletion_pending' THEN now()+($3::bigint*interval '1 day') END
         WHERE id=$1",
    )
    .bind(target)
    .bind(to)
    .bind(RECOVERABLE_DAYS)
    .execute(&mut *tx)
    .await?;
    let user = account(&mut tx, target).await?;
    audit(
        &mut tx,
        admin,
        action,
        &target.to_string(),
        json!({"email": user["email"], "from": status, "to": to}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(user))
}

#[derive(Deserialize)]
struct Change {
    status: String,
}

/// `PATCH /admin/users/{id}` `{status: active|disabled}`. Disabling signs the account out
/// everywhere; enabled again, it signs in afresh.
async fn change(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(change): Json<Change>,
) -> ApiResult<Json<Value>> {
    let auth = admin(&app, &headers).await?;
    match change.status.as_str() {
        "disabled" => {
            transition(
                &app,
                &auth,
                id,
                &["active"],
                "disabled",
                "admin.user.disable",
            )
            .await
        }
        "active" => {
            transition(
                &app,
                &auth,
                id,
                &["disabled"],
                "active",
                "admin.user.enable",
            )
            .await
        }
        _ => Err(Code::InvalidInput.into()),
    }
}

/// `DELETE /admin/users/{id}`: disable the account and delete it after [`RECOVERABLE_DAYS`];
/// until then it can be restored, and its email stays taken.
async fn delete(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let auth = admin(&app, &headers).await?;
    let from = ["active", "disabled"];
    transition(
        &app,
        &auth,
        id,
        &from,
        "deletion_pending",
        "admin.user.delete",
    )
    .await
}

/// `POST /admin/users/{id}/restore`: keep a deleted account, disabled.
async fn restore(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let auth = admin(&app, &headers).await?;
    let from = ["deletion_pending"];
    transition(&app, &auth, id, &from, "disabled", "admin.user.restore").await
}
