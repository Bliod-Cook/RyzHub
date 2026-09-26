use super::identity::secret;
use crate::{
    api::{ApiResult, App, authenticate},
    store::{Tx, audit},
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
pub async fn select(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Json(input): Json<CredentialSelection>,
) -> ApiResult<StatusCode> {
    let auth = authenticate(&app, &headers).await?;
    let ctx = app.store.context(auth.user_id, org).await?;
    let mut tx = app.store.tx(org).await?;
    let owner: Uuid = sqlx::query_scalar("SELECT creator_id FROM conversations WHERE org_id=$1 AND id=$2 AND state='active' FOR UPDATE").bind(org).bind(conv).fetch_optional(&mut *tx).await?.context("FORBIDDEN")?;
    authorize(
        &Principal::User(ctx.clone()),
        Action::UseConversation,
        &Resource {
            org_id: org,
            id: conv,
            owner_user_id: Some(owner),
            target_role: None,
        },
    )?;
    if let Some(id) = input.credential_id {
        let policy = super::gateway::effective_policy(&app, &mut tx, org).await?;
        let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM model_credentials WHERE org_id=$1 AND id=$2 AND revoked_at IS NULL AND (owner_user_id IS NULL OR (owner_user_id=$3 AND $4)))").bind(org).bind(id).bind(owner).bind(policy.allow_personal_credentials != Some(false)).fetch_one(&mut *tx).await?;
        crate::api::check(valid, "FORBIDDEN")?;
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
pub async fn put(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(input): Json<CredentialInput>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let ctx = app.store.context(auth.user_id, org).await?;
    authorize(
        &Principal::User(ctx.clone()),
        if input.organization {
            Action::SetOrgCredential
        } else {
            Action::SetPersonalCredential
        },
        &Resource {
            org_id: org,
            id: org,
            owner_user_id: None,
            target_role: None,
        },
    )?;
    crate::api::check(
        matches!(input.provider.as_str(), "anthropic" | "openai")
            && !input.name.trim().is_empty()
            && input.name.len() <= 100
            && !input.api_key.is_empty()
            && input.api_key.len() <= 4096,
        "INVALID_INPUT",
    )?;
    let mut tx = app.store.tx(org).await?;
    let policy = super::gateway::effective_policy(&app, &mut tx, org).await?;
    crate::api::check(
        input.organization || policy.allow_personal_credentials != Some(false),
        "FORBIDDEN",
    )?;
    let id = Uuid::now_v7();
    let owner = (!input.organization).then_some(auth.user_id);
    let encrypted = seal(
        &secret(&app.config.llm.vault_key_env)?,
        &aad(org, id, owner),
        &input.api_key,
    )?;
    sqlx::query("INSERT INTO model_credentials(id,org_id,owner_user_id,name,provider,encrypted_key) VALUES($1,$2,$3,$4,$6,$5)").bind(id).bind(org).bind(owner).bind(input.name.trim()).bind(encrypted).bind(&input.provider).execute(&mut *tx).await?;
    audit(
        &mut tx,
        &ctx,
        "credential.create",
        &id.to_string(),
        "allowed",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({"id":id,"name":input.name,"scope":if input.organization {"organization"}else{"personal"}}),
    ))
}
pub async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<Vec<Value>>> {
    let auth = authenticate(&app, &headers).await?;
    let _ctx = app.store.context(auth.user_id, org).await?;
    let mut tx = app.store.tx(org).await?;
    let rows=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'name',name,'provider',provider,'scope',CASE WHEN owner_user_id IS NULL THEN 'organization' ELSE 'personal' END) FROM model_credentials WHERE org_id=$1 AND (owner_user_id=$2 OR owner_user_id IS NULL) AND revoked_at IS NULL ORDER BY created_at DESC").bind(org).bind(auth.user_id).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(rows))
}
pub async fn revoke(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let auth = authenticate(&app, &headers).await?;
    let ctx = app.store.context(auth.user_id, org).await?;
    let mut tx = app.store.tx(org).await?;
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT owner_user_id FROM model_credentials WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .context("FORBIDDEN")?;
    authorize(
        &Principal::User(ctx.clone()),
        if owner.is_some() {
            Action::SetPersonalCredential
        } else {
            Action::SetOrgCredential
        },
        &Resource {
            org_id: org,
            id,
            owner_user_id: owner,
            target_role: None,
        },
    )?;
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
pub async fn resolve(
    app: &App,
    tx: &mut Tx<'_>,
    org: Uuid,
    user: Uuid,
    explicit: Option<Uuid>,
    policy: &ryzhub_domain::Policy,
    provider: &str,
) -> anyhow::Result<(Option<Uuid>, String)> {
    let row:Option<(Uuid,Option<Uuid>,Vec<u8>)>=sqlx::query_as("SELECT id,owner_user_id,encrypted_key FROM model_credentials WHERE org_id=$1 AND provider=$5 AND revoked_at IS NULL AND (($3::uuid IS NOT NULL AND id=$3) OR $3 IS NULL) AND (owner_user_id IS NULL OR (owner_user_id=$2 AND $4)) ORDER BY owner_user_id NULLS LAST,created_at DESC LIMIT 1").bind(org).bind(user).bind(explicit).bind(policy.allow_personal_credentials!=Some(false)).bind(provider).fetch_optional(&mut **tx).await?;
    if let Some((id, owner, cipher)) = row {
        return Ok((
            Some(id),
            unseal(
                &secret(&app.config.llm.vault_key_env)?,
                &aad(org, id, owner),
                &cipher,
            )?,
        ));
    }
    ensure!(explicit.is_none(), "CREDENTIAL_UNAVAILABLE");
    ensure!(
        policy.allow_platform_credentials != Some(false),
        "PLATFORM_CREDENTIAL_DENIED"
    );
    Ok((
        None,
        std::env::var(if provider == "openai" {
            &app.config.llm.responses_key_env
        } else {
            &app.config.llm.platform_key_env
        })
        .context("CREDENTIAL_UNAVAILABLE")?,
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
