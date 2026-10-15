//! PostgreSQL access. Every tenant query runs in a transaction that sets `app.org_id`, so row
//! level security backs up the explicit `org_id` predicates.
mod commands;
mod conversations;
mod events;
mod rows;

pub use commands::{Cid, finish_command, retry, take_cid};
pub use conversations::{create_in, list_conversations};
pub use events::{
    MAX_DETAIL_FIELD, MAX_ITEM_TEXT, append, conversation_item, events_between, head_seq,
    lock_lease, publish_conversation, truncate_utf8, upsert_item,
};
pub use rows::{ActionRow, Conversation, ItemRow, load_item};

use anyhow::{Context, ensure};
use chrono::{DateTime, Utc};
use ryzhub_domain::{Action, OrgContext, Principal, Resource, Role, authorize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::error::Code;

pub type Tx<'a> = Transaction<'a, Postgres>;

#[derive(Clone)]
pub struct Store {
    pub pool: PgPool,
}

#[derive(Clone, Debug)]
pub struct Auth {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub is_instance_admin: bool,
}

/// Exclusive right to write one conversation channel, fenced by `epoch`.
#[derive(Clone, Debug)]
pub struct Lease {
    pub org_id: Uuid,
    pub conversation_id: Uuid,
    pub owner_id: Uuid,
    pub epoch: i64,
}

impl Lease {
    pub fn channel(&self) -> String {
        format!("conv:{}", self.conversation_id)
    }
}

macro_rules! session_query {
    ($predicate:literal) => {
        concat!(
            "SELECT s.id,s.user_id,s.expires_at,u.is_instance_admin FROM auth_sessions s
             JOIN users u ON u.id=s.user_id WHERE ",
            $predicate,
            " AND s.revoked_at IS NULL AND s.expires_at>now() AND u.status='active'"
        )
    };
}

fn auth_row(row: Option<sqlx::postgres::PgRow>) -> anyhow::Result<Auth> {
    let row = row.context(Code::Unauthorized)?;
    Ok(Auth {
        session_id: row.get("id"),
        user_id: row.get("user_id"),
        expires_at: row.get("expires_at"),
        is_instance_admin: row.get("is_instance_admin"),
    })
}

pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

pub fn new_token() -> String {
    use base64::Engine;
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl Store {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(20)
            .connect(url)
            .await?;
        let privileged: bool = sqlx::query_scalar(
            "SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname=current_user",
        )
        .fetch_one(&pool)
        .await?;
        ensure!(
            !privileged,
            "runtime DATABASE_URL must use a non-superuser, non-BYPASSRLS role"
        );
        Ok(Self { pool })
    }

    /// A transaction scoped to one organization's rows.
    pub async fn tx(&self, org: Uuid) -> anyhow::Result<Tx<'_>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT set_config('app.org_id',$1,true)")
            .bind(org.to_string())
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    pub async fn authenticate(&self, token: &str) -> anyhow::Result<Auth> {
        let row = sqlx::query(session_query!("s.token_hash=$1"))
            .bind(hash_token(token))
            .fetch_optional(&self.pool)
            .await?;
        auth_row(row)
    }

    pub async fn auth_by_session(&self, id: Uuid) -> anyhow::Result<Auth> {
        let row = sqlx::query(session_query!("s.id=$1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        auth_row(row)
    }

    /// Whether session `id` was revoked or its user disabled; an expired session was not.
    pub async fn session_revoked(&self, id: Uuid) -> anyhow::Result<bool> {
        let revoked: Option<bool> = sqlx::query_scalar(
            "SELECT s.revoked_at IS NOT NULL OR u.status<>'active'
             FROM auth_sessions s JOIN users u ON u.id=s.user_id WHERE s.id=$1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(revoked.unwrap_or(true))
    }

    /// The user's live membership in `org`.
    pub async fn context(&self, user: Uuid, org: Uuid) -> anyhow::Result<OrgContext> {
        let mut tx = self.tx(org).await?;
        let ctx = context_in(&mut tx, user, org).await?;
        tx.commit().await?;
        Ok(ctx)
    }

    pub async fn conversation(&self, ctx: &OrgContext, id: Uuid) -> anyhow::Result<Conversation> {
        let mut tx = self.tx(ctx.org_id).await?;
        let conversation = conversation_in(&mut tx, ctx, id).await?;
        tx.commit().await?;
        Ok(conversation)
    }
}

/// Membership is read afresh on every entry point; roles are never cached.
pub async fn context_in(tx: &mut Tx<'_>, user: Uuid, org: Uuid) -> anyhow::Result<OrgContext> {
    // TODO(org): organizations are never suspended or deleted yet.
    let role = sqlx::query_scalar!(
        "SELECT m.role FROM memberships m
         JOIN organizations o ON o.id=m.org_id JOIN users u ON u.id=m.user_id
         WHERE m.org_id=$1 AND m.user_id=$2 AND o.state='active' AND u.status='active'",
        org,
        user
    )
    .fetch_optional(&mut **tx)
    .await?;
    let role = match role.as_deref() {
        Some("owner") => Role::Owner,
        Some("admin") => Role::Admin,
        Some("member") => Role::Member,
        _ => anyhow::bail!(Code::Forbidden),
    };
    Ok(OrgContext {
        org_id: org,
        user_id: user,
        role,
    })
}

/// A visible (active or archived) conversation the caller may use.
pub async fn conversation_in(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
    id: Uuid,
) -> anyhow::Result<Conversation> {
    let conversation: Conversation = sqlx::query_as(concat!(
        "SELECT ",
        conversation_columns!(),
        " FROM conversations WHERE org_id=$1 AND id=$2 AND state IN ('active','archived')"
    ))
    .bind(ctx.org_id)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .context(Code::Forbidden)?;
    authorize(
        &Principal::User(ctx.clone()),
        Action::UseConversation,
        &Resource {
            org_id: conversation.org_id,
            id: conversation.id,
            owner_user_id: Some(conversation.creator_id),
            target_role: None,
        },
    )
    .map_err(|_| Code::Forbidden)?;
    Ok(conversation)
}

pub async fn audit(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
    action: &str,
    resource: &str,
    result: &str,
) -> anyhow::Result<()> {
    audit_as(
        tx,
        ctx.org_id,
        ctx.user_id,
        "user",
        action,
        resource,
        result,
    )
    .await
}

pub async fn audit_as(
    tx: &mut Tx<'_>,
    org: Uuid,
    actor: Uuid,
    actor_kind: &str,
    action: &str,
    resource: &str,
    result: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO audit_logs(id,org_id,actor_id,actor_kind,action,resource,result)
         VALUES($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(Uuid::now_v7())
    .bind(org)
    .bind(actor)
    .bind(actor_kind)
    .bind(action)
    .bind(resource)
    .bind(result)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Hold the lock named `key` until `tx` ends.
pub async fn advisory_lock(tx: &mut Tx<'_>, key: &str) -> anyhow::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Latest organization policy restricted by the instance policy.
pub async fn effective_policy(
    tx: &mut Tx<'_>,
    instance: &ryzhub_domain::Policy,
    org: Uuid,
) -> anyhow::Result<ryzhub_domain::Policy> {
    // TODO(org): only registration writes an (empty) organization policy so far.
    let policy: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT policy FROM org_policies WHERE org_id=$1 ORDER BY version DESC LIMIT 1",
    )
    .bind(org)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(instance.restrict(&serde_json::from_value(
        policy.unwrap_or_else(|| serde_json::json!({})),
    )?))
}
