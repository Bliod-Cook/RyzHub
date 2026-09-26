use crate::{config::LlmConfig, store::Lease};
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub aud: String,
    pub kind: String,
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub conversation_id: Uuid,
    pub run_id: Uuid,
    pub exec_epoch: Option<i64>,
    pub sandbox_id: Option<Uuid>,
    pub lease_epoch: Option<i64>,
    pub credential_id: Option<Uuid>,
    pub models: Vec<String>,
    pub jti: Uuid,
    pub exp: i64,
}
pub fn secret(name: &str) -> anyhow::Result<Vec<u8>> {
    let encoded = std::env::var(name).context("MISSING_GATEWAY_SECRET")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded.trim())
        .context("INVALID_GATEWAY_SECRET")?;
    ensure!(bytes.len() == 32, "INVALID_GATEWAY_SECRET");
    Ok(bytes)
}
pub fn sign(config: &LlmConfig, claims: &Claims) -> anyhow::Result<String> {
    sign_with(&secret(&config.signing_key_env)?, claims)
}
fn sign_with(key: &[u8], claims: &Claims) -> anyhow::Result<String> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?);
    let content = format!("{header}.{payload}");
    let mut mac = Hmac::<Sha256>::new_from_slice(key)?;
    mac.update(content.as_bytes());
    Ok(format!(
        "{content}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    ))
}
pub fn verify(config: &LlmConfig, token: &str) -> anyhow::Result<Claims> {
    verify_with(&secret(&config.signing_key_env)?, token)
}
fn verify_with(key: &[u8], token: &str) -> anyhow::Result<Claims> {
    ensure!(token.len() <= 8192, "UNAUTHORIZED");
    let (content, signature) = token.rsplit_once('.').context("UNAUTHORIZED")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key)?;
    mac.update(content.as_bytes());
    mac.verify_slice(&URL_SAFE_NO_PAD.decode(signature)?)
        .context("UNAUTHORIZED")?;
    let (header, payload) = content.split_once('.').context("UNAUTHORIZED")?;
    let header: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header)?)?;
    ensure!(
        header["alg"] == "HS256" && header["typ"] == "JWT",
        "UNAUTHORIZED"
    );
    let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload)?)?;
    ensure!(
        claims.iss == "ryzhub"
            && claims.aud == "llm-gateway"
            && claims.exp > chrono::Utc::now().timestamp(),
        "UNAUTHORIZED"
    );
    Ok(claims)
}
pub fn loop_identity(lease: &Lease, user: Uuid, run: Uuid, config: &LlmConfig) -> Claims {
    Claims {
        iss: "ryzhub".into(),
        aud: "llm-gateway".into(),
        kind: "loop".into(),
        org_id: lease.org_id,
        user_id: user,
        conversation_id: lease.conversation_id,
        run_id: run,
        exec_epoch: Some(lease.epoch),
        sandbox_id: None,
        lease_epoch: None,
        credential_id: None,
        models: vec![config.model.clone()],
        jti: Uuid::now_v7(),
        exp: chrono::Utc::now().timestamp() + 300,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jwt_rejects_tampering_expiry_wrong_audience_and_key() {
        let lease = Lease {
            org_id: Uuid::now_v7(),
            conversation_id: Uuid::now_v7(),
            owner_id: Uuid::now_v7(),
            epoch: 2,
        };
        let mut c = loop_identity(
            &lease,
            Uuid::now_v7(),
            Uuid::now_v7(),
            &LlmConfig::default(),
        );
        let key = [7; 32];
        let token = sign_with(&key, &c).unwrap();
        assert_eq!(verify_with(&key, &token).unwrap().exec_epoch, Some(2));
        assert!(verify_with(&[8; 32], &token).is_err());
        assert!(verify_with(&key, &format!("{token}x")).is_err());
        c.exp = 0;
        assert!(verify_with(&key, &sign_with(&key, &c).unwrap()).is_err());
        c.exp = chrono::Utc::now().timestamp() + 100;
        c.aud = "other".into();
        assert!(verify_with(&key, &sign_with(&key, &c).unwrap()).is_err());
    }
}
