//! Conversations archived by policy when idle, and their stored workspaces deleted once the
//! archive has been kept long enough, without racing unarchiving or a restore in progress.
use super::*;

async fn settings(h: &Harness, user: &Value, conv: Uuid, body: Value) -> anyhow::Result<()> {
    reqwest::Client::new()
        .patch(format!(
            "{}/orgs/{}/conversations/{conv}/settings",
            h.base,
            id(user, "org_id")
        ))
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn storage(h: &Harness, conv: Uuid) -> anyhow::Result<(String, Option<Uuid>)> {
    Ok(sqlx::query_as(
        "SELECT storage_state,checkpoint_id FROM workspace_heads WHERE conversation_id=$1",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?)
}

async fn state(h: &Harness, conv: Uuid) -> anyhow::Result<(String, bool)> {
    Ok(
        sqlx::query_as("SELECT state,archived_at IS NOT NULL FROM conversations WHERE id=$1")
            .bind(conv)
            .fetch_one(&h.db)
            .await?,
    )
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and an execution node"]
async fn idle_conversations_archive_and_expired_workspaces_are_deleted() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let worker = PgPool::connect(&h.worker_url).await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let (conv, pinned, recent) = (
        h.conversation(&user).await?,
        h.conversation(&user).await?,
        h.conversation(&user).await?,
    );
    let mut owner = h.owner()?;
    h.send(&user, conv, message("markercat")).await?;
    let first = h.wait_state(conv, "completed").await?;
    let mut settled = false;
    for _ in 0..400 {
        settled = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspace_heads h
               JOIN checkpoints c ON c.id=h.checkpoint_id
               WHERE h.conversation_id=$1 AND c.state='committed')
             AND NOT EXISTS(SELECT 1 FROM sandboxes WHERE conversation_id=$1 AND state<>'deleted')",
        )
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
        if settled {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(settled, "checkpointed, sandbox gone");
    let (_, head) = storage(&h, conv).await?;
    let head = head.context("head checkpoint")?;
    let key: String = sqlx::query_scalar("SELECT manifest->>'sha256' FROM checkpoints WHERE id=$1")
        .bind(head)
        .fetch_one(&h.db)
        .await?;
    let store =
        ryzhub_server::workspace::blob::BlobStore::configured(&h.app.config().work.storage_root)?
            .scoped(org);
    assert!(store.get(&key).await.is_ok());

    // Without a policy nothing happens, however old.
    let age = "UPDATE conversations SET created_at=now()-interval '2 hours' WHERE org_id=$1;
               UPDATE runs SET started_at=now()-interval '2 hours',ended_at=now()-interval '2 hours'
               WHERE org_id=$1";
    for statement in age.split(';') {
        sqlx::query(sqlx::AssertSqlSafe(statement.to_owned()))
            .bind(org)
            .execute(&h.db)
            .await?;
    }
    ryzhub_server::lifecycle::sweep(&h.app, &worker).await?;
    assert_eq!(state(&h, conv).await?, ("active".into(), false));

    sqlx::query(
        "UPDATE org_policies
         SET policy=policy||'{\"auto_archive_after_secs\":3600,\"archive_retention_secs\":3600}'
         WHERE org_id=$1",
    )
    .bind(org)
    .execute(&h.db)
    .await?;
    settings(&h, &user, pinned, json!({"pinned":true})).await?;
    sqlx::query("UPDATE conversations SET created_at=now() WHERE id=$1")
        .bind(recent)
        .execute(&h.db)
        .await?;
    ryzhub_server::lifecycle::sweep(&h.app, &worker).await?;
    assert_eq!(state(&h, conv).await?, ("archived".into(), true));
    assert_eq!(storage(&h, conv).await?, ("cold".into(), Some(head)));
    assert_eq!(state(&h, pinned).await?.0, "active", "pinned stays");
    assert_eq!(state(&h, recent).await?.0, "active", "recent stays");
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs WHERE org_id=$1 AND action='conversation.auto_archive'",
    )
    .bind(org)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(audited, 1);

    // Unarchiving restarts the retention period; an archive kept long enough loses its
    // stored workspace. A checkpoint pinned by a restore in progress stays until released.
    settings(&h, &user, conv, json!({"archive":false})).await?;
    assert_eq!(state(&h, conv).await?, ("active".into(), false));
    assert_eq!(storage(&h, conv).await?.0, "hot");
    settings(&h, &user, conv, json!({"archive":true})).await?;
    ryzhub_server::lifecycle::sweep(&h.app, &worker).await?;
    assert_eq!(
        storage(&h, conv).await?,
        ("cold".into(), Some(head)),
        "kept"
    );
    sqlx::query("UPDATE conversations SET archived_at=now()-interval '2 hours' WHERE id=$1")
        .bind(conv)
        .execute(&h.db)
        .await?;
    let pin = Uuid::now_v7();
    sqlx::query("INSERT INTO restore_pins(id,org_id,checkpoint_id) VALUES($1,$2,$3)")
        .bind(pin)
        .bind(org)
        .bind(head)
        .execute(&h.db)
        .await?;
    ryzhub_server::lifecycle::sweep(&h.app, &worker).await?;
    assert_eq!(storage(&h, conv).await?, ("purging".into(), None));
    let retired: String = sqlx::query_scalar("SELECT state FROM checkpoints WHERE id=$1")
        .bind(head)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(retired, "tombstoned");
    assert!(store.get(&key).await.is_ok(), "pinned object kept");
    sqlx::query("DELETE FROM restore_pins WHERE id=$1")
        .bind(pin)
        .execute(&h.db)
        .await?;
    ryzhub_server::lifecycle::sweep(&h.app, &worker).await?;
    assert_eq!(storage(&h, conv).await?, ("deleted".into(), None));
    assert!(store.get(&key).await.is_err(), "object deleted");
    let purges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs WHERE org_id=$1 AND action='workspace.purge'",
    )
    .bind(org)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(purges, 1, "audited once");
    let status: Value = reqwest::Client::new()
        .get(format!(
            "{}/orgs/{org}/conversations/{conv}/workspace",
            h.base
        ))
        .bearer_auth(user["token"].as_str().unwrap())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(status["storage_state"], "deleted");

    // The timeline stays; a Chat conversation unarchived starts from an empty workspace.
    settings(&h, &user, conv, json!({"archive":false})).await?;
    h.send(&user, conv, message("markercat")).await?;
    let second = h.wait_after(conv, first).await?;
    let result: Value = sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1")
        .bind(second)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(result["stdout_head"], "once", "fresh workspace");
    let items: i64 = sqlx::query_scalar("SELECT count(*) FROM items WHERE run_id=$1")
        .bind(first)
        .fetch_one(&h.db)
        .await?;
    assert!(items > 0);

    worker.close().await;
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}
