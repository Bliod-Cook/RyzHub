//! Disabled and deleted accounts.
//!
//! Disabling takes everything an account signs in and acts with ([`revoke`]). A deleted
//! account stays disabled and recoverable for [`RECOVERABLE_DAYS`]; then [`collector`]
//! anonymizes it and purges its personal organization, handing conversations and projects to
//! their own collectors. Usage and audit records keep its id.
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{app::App, error::Code, store::Tx};

/// How long a deleted account can be restored.
pub const RECOVERABLE_DAYS: i64 = 30;

/// Hold `user` active until the transaction ends: disabling the account waits for it, and once
/// the account is disabled this fails.
pub async fn hold_active(conn: &mut PgConnection, user: Uuid) -> anyhow::Result<()> {
    let active: Option<bool> =
        sqlx::query_scalar("SELECT status='active' FROM users WHERE id=$1 FOR SHARE")
            .bind(user)
            .fetch_optional(conn)
            .await?;
    require!(active == Some(true), Code::AccountDisabled);
    Ok(())
}

/// Take what active `user` signs in and acts with: sessions, devices, gateway keys and
/// sign-ins in progress. Call before the status changes; the change's revocation notice then
/// closes the account's connections and model streams, and the Owner ends its runs.
pub async fn revoke(tx: &mut Tx<'_>, user: Uuid) -> anyhow::Result<()> {
    let orgs: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM list_user_orgs($1)")
        .bind(user)
        .fetch_all(&mut **tx)
        .await?;
    for statement in [
        "UPDATE auth_sessions SET revoked_at=now() WHERE user_id=$1 AND revoked_at IS NULL",
        "UPDATE auth_refresh SET revoked_at=now() WHERE user_id=$1 AND revoked_at IS NULL",
        "DELETE FROM login_flows WHERE link_user=$1",
        "DELETE FROM login_codes WHERE user_id=$1",
    ] {
        sqlx::query(statement).bind(user).execute(&mut **tx).await?;
    }
    for org in orgs {
        scope(tx, org).await?;
        sqlx::query(
            "UPDATE virtual_keys SET revoked_at=now()
             WHERE org_id=$1 AND user_id=$2 AND revoked_at IS NULL",
        )
        .bind(org)
        .bind(user)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Scope `tx` to `org`'s rows from here on.
async fn scope(tx: &mut Tx<'_>, org: Uuid) -> anyhow::Result<()> {
    sqlx::query("SELECT set_config('app.org_id',$1,true)")
        .bind(org.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Owner-role loop: anonymize accounts whose recovery window ended, and purge the
/// organizations they leave.
pub async fn collector(app: App, worker: PgPool) -> anyhow::Result<()> {
    loop {
        let due: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM users WHERE status='deletion_pending' AND delete_after<=now()
             ORDER BY delete_after LIMIT 100",
        )
        .fetch_all(&app.store.pool)
        .await?;
        for user in due {
            if let Err(error) = expire(&app, &worker, user).await {
                tracing::warn!(%user, error = format!("{error:#}"), "account deletion deferred");
            }
        }
        let orgs: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM organizations WHERE state='deleting'")
                .fetch_all(&worker)
                .await?;
        for org in orgs {
            if let Err(error) = purge(&app, org).await {
                tracing::warn!(%org, error = format!("{error:#}"), "organization purge deferred");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

/// Anonymize `user` once their recovery window has ended: free the email for one no sign-up
/// can take (it has no `@`), forget how they signed in and what they kept for themselves,
/// leave other organizations, and start purging their personal one.
pub async fn expire(app: &App, worker: &PgPool, user: Uuid) -> anyhow::Result<()> {
    let orgs: Vec<(Uuid, bool)> = sqlx::query_as(
        "SELECT m.org_id,o.kind='personal' AND m.role='owner' FROM memberships m
         JOIN organizations o ON o.id=m.org_id WHERE m.user_id=$1",
    )
    .bind(user)
    .fetch_all(worker)
    .await?;
    let mut tx = app.store.pool.begin().await?;
    let anonymized = sqlx::query(
        "UPDATE users SET email='deleted:'||id,display_name='',password_hash='!',
           status='deleted',delete_after=NULL
         WHERE id=$1 AND status='deletion_pending' AND delete_after<=now()",
    )
    .bind(user)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if anonymized == 0 {
        return Ok(());
    }
    for table in ["user_identities", "push_devices"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id=$1"
        )))
        .bind(user)
        .execute(&mut *tx)
        .await?;
    }
    for (org, personal) in orgs {
        scope(&mut tx, org).await?;
        for table in ["drafts", "memory_preferences", "read_states"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE org_id=$1 AND user_id=$2"
            )))
            .bind(org)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        }
        if personal {
            sqlx::query("UPDATE organizations SET state='deleting' WHERE id=$1")
                .bind(org)
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query("DELETE FROM memberships WHERE org_id=$1 AND user_id=$2")
                .bind(org)
                .bind(user)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// One round of purging `org`, which is being deleted; true once it is done. Every step can be
/// repeated: each round asks afresh what is left.
pub async fn purge(app: &App, org: Uuid) -> anyhow::Result<bool> {
    let mut tx = app.store.tx(org).await?;
    let deleting: Option<bool> =
        sqlx::query_scalar("SELECT state='deleting' FROM organizations WHERE id=$1 FOR UPDATE")
            .bind(org)
            .fetch_optional(&mut *tx)
            .await?;
    if deleting != Some(true) {
        return Ok(false);
    }
    // Conversations, temporary ones too, go to the conversation collector; projects to the
    // project collector at once rather than after their own recovery window.
    sqlx::query(
        "UPDATE projects SET state='deletion_pending',
           deleted_at=least(coalesce(deleted_at,now()),now()-($2::bigint*interval '1 day'))
         WHERE org_id=$1 AND state<>'deleted'",
    )
    .bind(org)
    .bind(crate::projects::RECOVERABLE_DAYS)
    .execute(&mut *tx)
    .await?;
    let memories: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM memory_items WHERE org_id=$1 AND (content<>'' OR status<>'deleted')",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    crate::memory::purge_memories(&mut tx, org, &memories).await?;
    // Model credentials stay as tombstones: usage records point at them.
    for statement in [
        "UPDATE conversations SET state='deleted',memory_snapshot=NULL
         WHERE org_id=$1 AND state<>'deleted'",
        "DELETE FROM conversation_digests WHERE org_id=$1",
        "UPDATE virtual_keys SET revoked_at=now() WHERE org_id=$1 AND revoked_at IS NULL",
        // Installations, their versions, tools and connections go with their plugins.
        "DELETE FROM plugins WHERE org_id=$1",
        "UPDATE model_credentials SET revoked_at=coalesce(revoked_at,now()),name='',secret_id=NULL
         WHERE org_id=$1",
        "DELETE FROM secrets WHERE org_id=$1",
        "DELETE FROM search_chunks WHERE org_id=$1",
        "DELETE FROM search_docs WHERE org_id=$1",
        "DELETE FROM search_jobs WHERE org_id=$1",
        "DELETE FROM push_jobs WHERE org_id=$1",
        "DELETE FROM oauth_flows WHERE org_id=$1",
        "DELETE FROM events WHERE org_id=$1",
    ] {
        sqlx::query(statement).bind(org).execute(&mut *tx).await?;
    }
    // A direct upload is aborted before its row goes: the row holds the upload's id.
    let direct: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id,s3_upload_id FROM uploads WHERE org_id=$1 AND direct AND s3_upload_id IS NOT NULL",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    for (id, upload) in direct {
        let (store, key) = crate::attachments::uploads::direct_object(app, org, id)?;
        store.abort_direct(&key, &upload).await?;
    }
    let mut tx = app.store.tx(org).await?;
    sqlx::query("DELETE FROM uploads WHERE org_id=$1")
        .bind(org)
        .execute(&mut *tx)
        .await?;
    let waiting: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM conversations WHERE org_id=$1 AND (state<>'deleted' OR title<>''))
         OR EXISTS(SELECT 1 FROM projects WHERE org_id=$1 AND state<>'deleted')",
    )
    .bind(org)
    .fetch_one(&mut *tx)
    .await?;
    if waiting {
        tx.commit().await?;
        return Ok(false);
    }
    // Purged conversations no longer refer to files.
    for table in ["attachments", "chat_images"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE org_id=$1"
        )))
        .bind(org)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    // Whatever is still stored for the organization (files, checkpoints, parts) is a leftover.
    crate::workspace::stores(app, org)?.clear().await?;
    sqlx::query("UPDATE organizations SET state='deleted' WHERE id=$1 AND state='deleting'")
        .bind(org)
        .execute(&app.store.pool)
        .await?;
    Ok(true)
}
