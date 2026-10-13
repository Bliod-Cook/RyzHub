//! Envelope encryption for secrets.
//!
//! A secret is encrypted with its own random data key (DEK); the DEK is wrapped by the vault
//! key (KEK) configured as `llm.vault_key_env` and recorded by id. Rotating the vault key only
//! rewraps DEKs ([`rewrap`]); retired keys stay readable until then.
//!
//! The ciphertext is bound to its organization, row and `purpose`: the caller's description
//! of what owns it (for example a credential and its owner), so a secret cannot be moved to
//! another row or organization and still decrypt.
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use uuid::Uuid;

use crate::{config::LlmConfig, store::Tx};

/// A 32-byte key from an environment variable holding URL-safe base64.
pub fn secret(name: &str) -> anyhow::Result<Vec<u8>> {
    let encoded = std::env::var(name).with_context(|| format!("{name} is not set"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded.trim())
        .with_context(|| format!("{name} is not URL-safe base64"))?;
    ensure!(bytes.len() == 32, "{name} must hold 32 bytes");
    Ok(bytes)
}

/// The vault key with `id`: the current one, or a retired one kept for decryption.
pub(crate) fn kek(config: &LlmConfig, id: i16) -> anyhow::Result<Vec<u8>> {
    if id == config.vault_key_id {
        return secret(&config.vault_key_env);
    }
    let env = config
        .vault_retired_keys
        .get(&id.to_string())
        .with_context(|| format!("vault key {id} is not configured"))?;
    secret(env)
}

/// AES-256-GCM with a random nonce prepended to the ciphertext.
pub(crate) fn seal(key: &[u8], associated: &str, plain: &[u8]) -> anyhow::Result<Vec<u8>> {
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("INVALID_VAULT_KEY"))?;
    let nonce: [u8; 12] = rand::random();
    let encrypted = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plain,
                aad: associated.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("VAULT_ENCRYPT_FAILED"))?;
    Ok([nonce.as_slice(), &encrypted].concat())
}

pub(crate) fn unseal(key: &[u8], associated: &str, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    ensure!(bytes.len() >= 28, "VAULT_CORRUPT");
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("INVALID_VAULT_KEY"))?;
    cipher
        .decrypt(
            Nonce::from_slice(&bytes[..12]),
            Payload {
                msg: &bytes[12..],
                aad: associated.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("VAULT_DECRYPT_FAILED"))
}

fn data_aad(org: Uuid, id: Uuid, purpose: &str) -> String {
    format!("secret/{org}/{id}/{purpose}")
}

fn key_aad(org: Uuid, id: Uuid) -> String {
    format!("dek/{org}/{id}")
}

/// Encrypt `plain` for `purpose` and store it; returns the secret's id.
pub async fn put(
    tx: &mut Tx<'_>,
    config: &LlmConfig,
    org: Uuid,
    purpose: &str,
    plain: &[u8],
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    let dek: [u8; 32] = rand::random();
    let ciphertext = seal(&dek, &data_aad(org, id, purpose), plain)?;
    let wrapped = seal(&kek(config, config.vault_key_id)?, &key_aad(org, id), &dek)?;
    sqlx::query(
        "INSERT INTO secrets(id,org_id,ciphertext,wrapped_dek,kek_id) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(id)
    .bind(org)
    .bind(ciphertext)
    .bind(wrapped)
    .bind(config.vault_key_id)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

/// The plaintext of secret `id`, which must have been stored for `purpose`.
pub async fn get(
    tx: &mut Tx<'_>,
    config: &LlmConfig,
    org: Uuid,
    id: Uuid,
    purpose: &str,
) -> anyhow::Result<Vec<u8>> {
    let (ciphertext, wrapped, kek_id): (Vec<u8>, Vec<u8>, i16) = sqlx::query_as(
        "SELECT ciphertext,wrapped_dek,kek_id FROM secrets WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .context("VAULT_SECRET_MISSING")?;
    let dek = unseal(&kek(config, kek_id)?, &key_aad(org, id), &wrapped)?;
    unseal(&dek, &data_aad(org, id, purpose), &ciphertext)
}

/// Delete secret `id`. Its data key goes with it, so no copy of the row can be decrypted
/// afterwards without an old backup and vault key.
pub async fn delete(tx: &mut Tx<'_>, org: Uuid, id: Uuid) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM secrets WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Rewrap every data key of `org` that a retired vault key wraps; returns how many.
pub async fn rewrap(tx: &mut Tx<'_>, config: &LlmConfig, org: Uuid) -> anyhow::Result<u64> {
    let stale: Vec<(Uuid, Vec<u8>, i16)> = sqlx::query_as(
        "SELECT id,wrapped_dek,kek_id FROM secrets WHERE org_id=$1 AND kek_id<>$2 FOR UPDATE",
    )
    .bind(org)
    .bind(config.vault_key_id)
    .fetch_all(&mut **tx)
    .await?;
    let current = kek(config, config.vault_key_id)?;
    for (id, wrapped, kek_id) in &stale {
        let dek = unseal(&kek(config, *kek_id)?, &key_aad(org, *id), wrapped)?;
        sqlx::query("UPDATE secrets SET wrapped_dek=$3,kek_id=$4 WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(id)
            .bind(seal(&current, &key_aad(org, *id), &dek)?)
            .bind(config.vault_key_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(stale.len() as u64)
}

/// `ryzhub-server migrate-secrets`: move credentials still encrypted the old way into
/// secrets, then rewrap data keys under the current vault key. Each organization is handled
/// in its own transaction under its row-level-security scope, so the command works for a
/// migration role with or without BYPASSRLS. Returns (credentials converted, keys rewrapped).
pub async fn migrate(pool: &sqlx::PgPool, config: &LlmConfig) -> anyhow::Result<(u64, u64)> {
    let orgs: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM organizations ORDER BY id")
        .fetch_all(pool)
        .await?;
    let (mut converted, mut rewrapped) = (0, 0);
    for org in orgs {
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT set_config('app.org_id',$1,true)")
            .bind(org.to_string())
            .execute(&mut *tx)
            .await?;
        converted += crate::llm::credentials::convert_legacy(&mut tx, config, org).await?;
        rewrapped += rewrap(&mut tx, config, org).await?;
        tx.commit().await?;
    }
    Ok((converted, rewrapped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_bytes_are_bound_to_their_associated_data() {
        let key = [4; 32];
        let encrypted = seal(&key, "org/a/member", b"secret").unwrap();
        assert_eq!(unseal(&key, "org/a/member", &encrypted).unwrap(), b"secret");
        assert!(unseal(&key, "org/b/member", &encrypted).is_err());
        let mut tampered = encrypted;
        tampered[14] ^= 1;
        assert!(unseal(&key, "org/a/member", &tampered).is_err());
    }

    #[test]
    fn a_data_key_opens_only_its_own_row_and_purpose() {
        let (org, id, other) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let dek = [7; 32];
        let ciphertext = seal(&dek, &data_aad(org, id, "credential/x/"), b"k").unwrap();
        assert!(unseal(&dek, &data_aad(org, id, "credential/x/"), &ciphertext).is_ok());
        assert!(unseal(&dek, &data_aad(org, other, "credential/x/"), &ciphertext).is_err());
        assert!(unseal(&dek, &data_aad(org, id, "credential/y/"), &ciphertext).is_err());
        let wrapped = seal(&[9; 32], &key_aad(org, id), &dek).unwrap();
        assert!(unseal(&[9; 32], &key_aad(org, other), &wrapped).is_err());
    }
}
