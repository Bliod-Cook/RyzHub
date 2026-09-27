use crate::{
    app::App,
    config::LlmConfig,
    error::{ApiResult, Code},
    store::{Tx, audit, effective_policy},
};
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ryzhub_domain::{Action, Principal, Resource, authorize};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

fn aad(org: Uuid, id: Uuid, owner: Option<Uuid>) -> String {
    format!(
        "{org}/{id}/{}",
        owner.map(|u| u.to_string()).unwrap_or_default()
    )
}

/// A 32-byte key from an environment variable holding URL-safe base64.
pub fn secret(name: &str) -> anyhow::Result<Vec<u8>> {
    let encoded = std::env::var(name).with_context(|| format!("{name} is not set"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded.trim())
        .with_context(|| format!("{name} is not URL-safe base64"))?;
    ensure!(bytes.len() == 32, "{name} must hold 32 bytes");
    Ok(bytes)
}

/// The vault key a ciphertext was written with. New ciphertexts use `vault_key_id`; retired
/// keys stay readable until every credential has been re-encrypted.
fn vault_key(config: &LlmConfig, id: i16) -> anyhow::Result<Vec<u8>> {
    if id == config.vault_key_id {
        return secret(&config.vault_key_env);
    }
    let env = config
        .vault_retired_keys
        .get(&id.to_string())
        .with_context(|| format!("vault key {id} is not configured"))?;
    secret(env)
}

fn seal(key: &[u8], associated: &str, text: &str) -> anyhow::Result<Vec<u8>> {
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("INVALID_VAULT_KEY"))?;
    let nonce: [u8; 12] = rand::random();
    let encrypted = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: text.as_bytes(),
                aad: associated.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("VAULT_ENCRYPT_FAILED"))?;
    Ok([nonce.as_slice(), &encrypted].concat())
}

fn unseal(key: &[u8], associated: &str, bytes: &[u8]) -> anyhow::Result<String> {
    ensure!(bytes.len() >= 28, "VAULT_CORRUPT");
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("INVALID_VAULT_KEY"))?;
    let plain = cipher
        .decrypt(
            Nonce::from_slice(&bytes[..12]),
            Payload {
                msg: &bytes[12..],
                aad: associated.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("VAULT_DECRYPT_FAILED"))?;
    Ok(String::from_utf8(plain)?)
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
        matches!(input.provider.as_str(), "anthropic" | "openai")
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
    let key_id = app.config.llm.vault_key_id;
    let encrypted = seal(
        &vault_key(&app.config.llm, key_id)?,
        &aad(org, id, owner),
        &input.api_key,
    )?;
    sqlx::query(
        "INSERT INTO model_credentials(id,org_id,owner_user_id,name,provider,encrypted_key,key_id)
         VALUES($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(id)
    .bind(org)
    .bind(owner)
    .bind(input.name.trim())
    .bind(&input.provider)
    .bind(encrypted)
    .bind(key_id)
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
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT owner_user_id FROM model_credentials WHERE org_id=$1 AND id=$2")
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
    sqlx::query("UPDATE model_credentials SET revoked_at=now() WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(id)
        .execute(&mut *tx)
        .await?;
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
    let row: Option<(Uuid, Option<Uuid>, Vec<u8>, i16)> = sqlx::query_as(
        "SELECT id,owner_user_id,encrypted_key,key_id FROM model_credentials
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
    if let Some((id, owner, cipher, key_id)) = row {
        let key = vault_key(&app.config.llm, key_id)?;
        return Ok((Some(id), unseal(&key, &aad(org, id, owner), &cipher)?));
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vault_authenticates_tenant_owner_and_ciphertext() {
        let key = [4; 32];
        let encrypted = seal(&key, "org/a/member", "secret").unwrap();
        assert_eq!(unseal(&key, "org/a/member", &encrypted).unwrap(), "secret");
        assert!(unseal(&key, "org/b/member", &encrypted).is_err());
        let mut tampered = encrypted;
        tampered[14] ^= 1;
        assert!(unseal(&key, "org/a/member", &tampered).is_err());
    }
}
