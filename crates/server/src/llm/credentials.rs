use crate::{
    app::App,
    error::{ApiResult, Code},
    store::{Tx, audit, effective_policy},
    vault,
};
use anyhow::Context;
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use ryzhub_domain::{Action, Principal, Resource, authorize};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

/// What a credential's secret is bound to: the credential and its owner, so the secret cannot
/// be moved to another credential or change hands.
fn purpose(id: Uuid, owner: Option<Uuid>) -> String {
    format!(
        "credential/{id}/{}",
        owner.map(|u| u.to_string()).unwrap_or_default()
    )
}

fn default_provider() -> String {
    "anthropic".into()
}

#[derive(Deserialize)]
pub struct CredentialInput {
    pub name: String,
    pub api_key: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub organization: bool,
}

/// `POST /orgs/{org}/credentials`: store a personal or (owner/admin) organization key.
pub async fn put(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(input): Json<CredentialInput>,
) -> ApiResult<Json<Value>> {
    let ctx = crate::http::member(&app, &headers, org).await?;
    let action = if input.organization {
        Action::SetOrgCredential
    } else {
        Action::SetPersonalCredential
    };
    authorize(
        &Principal::User(ctx.clone()),
        action,
        &Resource {
            org_id: org,
            id: org,
            owner_user_id: None,
            target_role: None,
        },
    )
    .map_err(|_| Code::Forbidden)?;
    require!(
        matches!(input.provider.as_str(), "anthropic" | "openai" | "gemini")
            && !input.name.trim().is_empty()
            && input.name.len() <= 100
            && !input.api_key.is_empty()
            && input.api_key.len() <= 4096,
        Code::InvalidInput
    );
    let mut tx = app.store.tx(org).await?;
    let policy = effective_policy(&mut tx, &app.config.policy, org).await?;
    require!(
        input.organization || policy.allow_personal_credentials != Some(false),
        Code::Forbidden
    );
    let id = Uuid::now_v7();
    let owner = (!input.organization).then_some(ctx.user_id);
    let secret = vault::put(
        &mut tx,
        &app.config.llm,
        org,
        &purpose(id, owner),
        input.api_key.as_bytes(),
    )
    .await?;
    sqlx::query(
        "INSERT INTO model_credentials(id,org_id,owner_user_id,name,provider,secret_id)
         VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(id)
    .bind(org)
    .bind(owner)
    .bind(input.name.trim())
    .bind(&input.provider)
    .bind(secret)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        &ctx,
        "credential.create",
        &id.to_string(),
        "allowed",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "id": id,
        "name": input.name,
        "scope": if input.organization { "organization" } else { "personal" },
    })))
}

/// `GET /orgs/{org}/credentials`: metadata of the caller's and the organization's keys.
pub async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<Vec<Value>>> {
    let (ctx, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    let rows = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',id,'name',name,'provider',provider,
                'scope',CASE WHEN owner_user_id IS NULL THEN 'organization' ELSE 'personal' END)
         FROM model_credentials WHERE org_id=$1 AND (owner_user_id=$2 OR owner_user_id IS NULL)
           AND revoked_at IS NULL ORDER BY created_at DESC",
    )
    .bind(org)
    .bind(ctx.user_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(rows))
}

/// `DELETE /orgs/{org}/credentials/{id}`: revoke a key. Model streams using it end through the
/// revocation notice.
pub async fn revoke(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let (ctx, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    let (owner, secret): (Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT owner_user_id,secret_id FROM model_credentials WHERE org_id=$1 AND id=$2 FOR UPDATE",
    )
    .bind(org)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .context(Code::Forbidden)?;
    let action = if owner.is_some() {
        Action::SetPersonalCredential
    } else {
        Action::SetOrgCredential
    };
    authorize(
        &Principal::User(ctx.clone()),
        action,
        &Resource {
            org_id: org,
            id,
            owner_user_id: owner,
            target_role: None,
        },
    )
    .map_err(|_| Code::Forbidden)?;
    // The key is no longer needed by anything: drop it rather than keep it encrypted.
    sqlx::query(
        "UPDATE model_credentials SET revoked_at=coalesce(revoked_at,now()),secret_id=NULL
         WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if let Some(secret) = secret {
        vault::delete(&mut tx, org, secret).await?;
    }
    audit(
        &mut tx,
        &ctx,
        "credential.revoke",
        &id.to_string(),
        "allowed",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The member's or organization's own key for `provider`: the member's, else the
/// organization's. None when there is none: the platform's keys apply.
pub async fn member_key(
    app: &App,
    tx: &mut Tx<'_>,
    org: Uuid,
    user: Uuid,
    policy: &ryzhub_domain::Policy,
    provider: &str,
) -> anyhow::Result<Option<(Uuid, String)>> {
    let row: Option<(Uuid, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT id,owner_user_id,secret_id FROM model_credentials
         WHERE org_id=$1 AND provider=$4 AND revoked_at IS NULL
           AND (owner_user_id IS NULL OR (owner_user_id=$2 AND $3))
         ORDER BY owner_user_id NULLS LAST,created_at DESC LIMIT 1",
    )
    .bind(org)
    .bind(user)
    .bind(policy.allow_personal_credentials != Some(false))
    .bind(provider)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((id, owner, secret)) = row else {
        return Ok(None);
    };
    let secret = secret.context(Code::CredentialUnavailable)?;
    let key = vault::get(tx, &app.config.llm, org, secret, &purpose(id, owner)).await?;
    Ok(Some((id, String::from_utf8(key)?)))
}
