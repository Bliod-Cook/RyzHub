use crate::{
    app::App,
    config::LlmConfig,
    error::{ApiResult, Code},
    store::{Tx, audit, effective_policy},
    vault,
};
use anyhow::{Context, ensure};
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

/// Associated data of a credential encrypted before secrets existed.
fn legacy_aad(org: Uuid, id: Uuid, owner: Option<Uuid>) -> String {
    format!(
        "{org}/{id}/{}",
        owner.map(|u| u.to_string()).unwrap_or_default()
    )
}

fn open_legacy(
    config: &LlmConfig,
    org: Uuid,
    id: Uuid,
    owner: Option<Uuid>,
    cipher: &[u8],
    key_id: i16,
) -> anyhow::Result<String> {
    let key = vault::kek(config, key_id)?;
    Ok(String::from_utf8(vault::unseal(
        &key,
        &legacy_aad(org, id, owner),
        cipher,
    )?)?)
}

/// Move the organization's credentials that are still encrypted the old way into secrets,
/// and drop the ciphertext of revoked ones. Returns how many were moved.
pub async fn convert_legacy(tx: &mut Tx<'_>, config: &LlmConfig, org: Uuid) -> anyhow::Result<u64> {
    let legacy: Vec<(Uuid, Option<Uuid>, Vec<u8>, i16)> = sqlx::query_as(
        "SELECT id,owner_user_id,encrypted_key,key_id FROM model_credentials
         WHERE org_id=$1 AND encrypted_key IS NOT NULL AND revoked_at IS NULL FOR UPDATE",
    )
    .bind(org)
    .fetch_all(&mut **tx)
    .await?;
    for (id, owner, cipher, key_id) in &legacy {
        let plain = open_legacy(config, org, *id, *owner, cipher, *key_id)?;
        let secret = vault::put(tx, config, org, &purpose(*id, *owner), plain.as_bytes()).await?;
        sqlx::query(
            "UPDATE model_credentials SET secret_id=$3,encrypted_key=NULL WHERE org_id=$1 AND id=$2",
        )
        .bind(org)
        .bind(id)
        .bind(secret)
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query(
        "UPDATE model_credentials SET encrypted_key=NULL
         WHERE org_id=$1 AND revoked_at IS NOT NULL AND encrypted_key IS NOT NULL",
    )
    .bind(org)
    .execute(&mut **tx)
    .await?;
    Ok(legacy.len() as u64)
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

#[derive(Deserialize)]
pub struct CredentialSelection {
    pub credential_id: Option<Uuid>,
}

/// A credential the conversation's creator may use under current policy.
async fn usable(
    tx: &mut Tx<'_>,
    policy: &ryzhub_domain::Policy,
    org: Uuid,
    id: Uuid,
    user: Uuid,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM model_credentials WHERE org_id=$1 AND id=$2 AND revoked_at IS NULL
         AND (owner_user_id IS NULL OR (owner_user_id=$3 AND $4)))",
    )
    .bind(org)
    .bind(id)
    .bind(user)
    .bind(policy.allow_personal_credentials != Some(false))
    .fetch_one(&mut **tx)
    .await?)
}

/// `POST /orgs/{org}/conversations/{conv}/model-credential`: pin (or clear) the credential for
/// the conversation's future runs.
pub async fn select(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Json(input): Json<CredentialSelection>,
) -> ApiResult<StatusCode> {
    let (ctx, mut tx) = crate::http::member_tx(&app, &headers, org).await?;
    let owner: Uuid = sqlx::query_scalar(
        "SELECT creator_id FROM conversations WHERE org_id=$1 AND id=$2 AND state='active' FOR UPDATE",
    )
    .bind(org)
    .bind(conv)
    .fetch_optional(&mut *tx)
    .await?
    .context(Code::Forbidden)?;
    authorize(
        &Principal::User(ctx.clone()),
        Action::UseConversation,
        &Resource {
            org_id: org,
            id: conv,
            owner_user_id: Some(owner),
            target_role: None,
        },
    )
    .map_err(|_| Code::Forbidden)?;
    if let Some(id) = input.credential_id {
        let policy = effective_policy(&mut tx, &app.config.policy, org).await?;
        require!(
            usable(&mut tx, &policy, org, id, owner).await?,
            Code::Forbidden
        );
    }
    sqlx::query("UPDATE conversations SET model_credential_id=$3 WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(conv)
        .bind(input.credential_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &ctx,
        "credential.select",
        &conv.to_string(),
        "allowed",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
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

/// `DELETE /orgs/{org}/credentials/{id}`: revoke a key and every gateway key bound to it.
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
        "UPDATE model_credentials SET revoked_at=coalesce(revoked_at,now()),secret_id=NULL,encrypted_key=NULL
         WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if let Some(secret) = secret {
        vault::delete(&mut tx, org, secret).await?;
    }
    sqlx::query("UPDATE virtual_keys SET revoked_at=now() WHERE org_id=$1 AND credential_id=$2")
        .bind(org)
        .bind(id)
        .execute(&mut *tx)
        .await?;
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

/// A credential's key as stored: in a secret, or (before `migrate-secrets`) encrypted in place.
#[derive(sqlx::FromRow)]
struct StoredKey {
    id: Uuid,
    owner_user_id: Option<Uuid>,
    secret_id: Option<Uuid>,
    encrypted_key: Option<Vec<u8>>,
    key_id: i16,
}

/// The upstream key for a request: the explicitly selected credential, else the user's
/// personal key, else the organization's, else the platform key when policy allows it.
/// An explicit selection never falls back.
pub async fn resolve(
    app: &App,
    tx: &mut Tx<'_>,
    org: Uuid,
    user: Uuid,
    explicit: Option<Uuid>,
    policy: &ryzhub_domain::Policy,
    provider: &str,
) -> anyhow::Result<(Option<Uuid>, String)> {
    if let Some((id, key)) = member_key(app, tx, org, user, explicit, policy, provider).await? {
        return Ok((Some(id), key));
    }
    ensure!(explicit.is_none(), Code::CredentialUnavailable);
    ensure!(
        policy.allow_platform_credentials != Some(false),
        Code::PlatformCredentialDenied
    );
    let env = if provider == "openai" {
        &app.config.llm.responses_key_env
    } else {
        &app.config.llm.platform_key_env
    };
    Ok((
        None,
        std::env::var(env).context(Code::CredentialUnavailable)?,
    ))
}

/// The member's or organization's own key for `provider`: `explicit` when given, else the
/// member's, else the organization's. None when there is none: the platform's keys apply.
pub async fn member_key(
    app: &App,
    tx: &mut Tx<'_>,
    org: Uuid,
    user: Uuid,
    explicit: Option<Uuid>,
    policy: &ryzhub_domain::Policy,
    provider: &str,
) -> anyhow::Result<Option<(Uuid, String)>> {
    let row: Option<StoredKey> = sqlx::query_as(
        "SELECT id,owner_user_id,secret_id,encrypted_key,key_id FROM model_credentials
         WHERE org_id=$1 AND provider=$5 AND revoked_at IS NULL AND ($3::uuid IS NULL OR id=$3)
           AND (owner_user_id IS NULL OR (owner_user_id=$2 AND $4))
         ORDER BY owner_user_id NULLS LAST,created_at DESC LIMIT 1",
    )
    .bind(org)
    .bind(user)
    .bind(explicit)
    .bind(policy.allow_personal_credentials != Some(false))
    .bind(provider)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let (config, id, owner) = (&app.config.llm, row.id, row.owner_user_id);
    let key = match (row.secret_id, row.encrypted_key) {
        (Some(secret), _) => {
            String::from_utf8(vault::get(tx, config, org, secret, &purpose(id, owner)).await?)?
        }
        (None, Some(cipher)) => open_legacy(config, org, id, owner, &cipher, row.key_id)?,
        (None, None) => anyhow::bail!(Code::CredentialUnavailable),
    };
    Ok(Some((id, key)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_credential_secret_is_bound_to_its_owner() {
        let (id, owner) = (Uuid::now_v7(), Uuid::now_v7());
        assert_ne!(purpose(id, Some(owner)), purpose(id, None));
        assert_ne!(purpose(id, None), purpose(Uuid::now_v7(), None));
    }
}
