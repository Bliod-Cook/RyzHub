//! Opaque bearer keys for the LLM and MCP gateways.
//!
//! A key is 256 random bits; only its SHA-256 is stored. The row carries everything the gateway
//! needs (tenant, run, binding and allowed models), so there is nothing to sign and every
//! request sees revocation immediately.
//!
//! - `loop` keys are issued by the Owner for one Chat model call and are fenced by the run's
//!   execution epoch.
//! - `sandbox` keys live only in the guest's memory, are bound to the sandbox lease epoch
//!   (not the Owner epoch, so an Owner takeover does not interrupt a harness) and rotate hourly.
use anyhow::{Context, ensure};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    error::Code,
    store::{Lease, Store, context_in, conversation_in, hash_token, new_token},
};

const LOOP_TTL_SECS: i64 = 600;
const SANDBOX_TTL_SECS: i64 = 3600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    Loop,
    Sandbox,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct GatewayKey {
    pub id: Uuid,
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub conversation_id: Uuid,
    pub run_id: Uuid,
    kind: String,
    pub exec_epoch: Option<i64>,
    pub sandbox_id: Option<Uuid>,
    pub lease_epoch: Option<i64>,
    pub models: Vec<String>,
    pub credential_id: Option<Uuid>,
    pub expires_at: DateTime<Utc>,
}

impl GatewayKey {
    pub fn kind(&self) -> KeyKind {
        if self.kind == "loop" {
            KeyKind::Loop
        } else {
            KeyKind::Sandbox
        }
    }
}

/// A key for one Owner model call on `run`.
pub async fn issue_loop_key(
    store: &Store,
    lease: &Lease,
    user: Uuid,
    run: Uuid,
    models: Vec<String>,
) -> anyhow::Result<String> {
    let token = new_token();
    let mut tx = store.tx(lease.org_id).await?;
    sqlx::query(
        "INSERT INTO virtual_keys(id,org_id,user_id,conversation_id,run_id,kind,exec_epoch,models,token_hash,expires_at)
         VALUES($1,$2,$3,$4,$5,'loop',$6,$7,$8,now()+($9::bigint * interval '1 second'))",
    )
    .bind(Uuid::now_v7())
    .bind(lease.org_id)
    .bind(user)
    .bind(lease.conversation_id)
    .bind(run)
    .bind(lease.epoch)
    .bind(&models)
    .bind(hash_token(&token))
    .bind(LOOP_TTL_SECS)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(token)
}

pub struct SandboxKeyRequest {
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub conversation_id: Uuid,
    pub run_id: Uuid,
    pub sandbox_id: Uuid,
    pub lease_epoch: i64,
    pub models: Vec<String>,
    pub credential_id: Option<Uuid>,
}

pub struct IssuedKey {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

/// Issue a sandbox key and supersede the sandbox's previous keys. Streams already accepted
/// with a superseded key continue; new requests need the new key.
pub async fn issue_sandbox_key(
    store: &Store,
    request: SandboxKeyRequest,
) -> anyhow::Result<IssuedKey> {
    let token = new_token();
    let mut tx = store.tx(request.org_id).await?;
    let ctx = context_in(&mut tx, request.user_id, request.org_id).await?;
    conversation_in(&mut tx, &ctx, request.conversation_id).await?;
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sandboxes WHERE org_id=$1 AND id=$2 AND run_id=$3
         AND conversation_id=$4 AND lease_epoch=$5 AND state='running')",
    )
    .bind(request.org_id)
    .bind(request.sandbox_id)
    .bind(request.run_id)
    .bind(request.conversation_id)
    .bind(request.lease_epoch)
    .fetch_one(&mut *tx)
    .await?;
    ensure!(live, Code::Forbidden);
    sqlx::query(
        "UPDATE virtual_keys SET superseded_at=now() WHERE org_id=$1 AND sandbox_id=$2 AND superseded_at IS NULL",
    )
    .bind(request.org_id)
    .bind(request.sandbox_id)
    .execute(&mut *tx)
    .await?;
    let expires_at: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO virtual_keys(id,org_id,user_id,conversation_id,run_id,kind,sandbox_id,lease_epoch,models,
             credential_id,token_hash,expires_at)
         VALUES($1,$2,$3,$4,$5,'sandbox',$6,$7,$8,$9,$10,now()+($11::bigint * interval '1 second'))
         RETURNING expires_at",
    )
    .bind(Uuid::now_v7())
    .bind(request.org_id)
    .bind(request.user_id)
    .bind(request.conversation_id)
    .bind(request.run_id)
    .bind(request.sandbox_id)
    .bind(request.lease_epoch)
    .bind(&request.models)
    .bind(request.credential_id)
    .bind(hash_token(&token))
    .bind(SANDBOX_TTL_SECS)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(IssuedKey { token, expires_at })
}

/// Look a presented token up. Liveness and authorization are checked by the gateway.
pub async fn resolve(store: &Store, token: &str) -> anyhow::Result<GatewayKey> {
    ensure!(!token.is_empty() && token.len() <= 256, Code::Unauthorized);
    let found = sqlx::query!(
        r#"SELECT id AS "id!",org_id AS "org_id!" FROM resolve_gateway_key($1)"#,
        hash_token(token)
    )
    .fetch_optional(&store.pool)
    .await?
    .context(Code::Unauthorized)?;
    let (id, org) = (found.id, found.org_id);
    let mut tx = store.tx(org).await?;
    let key: GatewayKey = sqlx::query_as(
        "SELECT id,org_id,user_id,conversation_id,run_id,kind,exec_epoch,sandbox_id,lease_epoch,models,
                credential_id,expires_at
         FROM virtual_keys WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(key)
}
