//! Shared by the suites that use a real execution node.
use std::path::Path;

use ryzhub_execution::HostCommand;
use sqlx::PgPool;
use uuid::Uuid;

/// The execution node's socket the suites use.
pub fn host_socket() -> std::path::PathBuf {
    std::env::var_os("TEST_HOST_SOCKET").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../var/execution/host.sock"),
        Into::into,
    )
}

/// Take a test database's sandboxes and workspace copies off the execution node. A sandbox no
/// control plane knows about any more would otherwise run until the node runs out of
/// processes.
pub async fn release(db: &PgPool, host_socket: &Path) {
    let Ok(sandboxes) = sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
        "SELECT id,org_id,conversation_id FROM sandboxes WHERE state<>'deleted'",
    )
    .fetch_all(db)
    .await
    else {
        return;
    };
    let mut workspaces = Vec::new();
    for (id, org, conv) in sandboxes {
        let destroy = HostCommand::Destroy {
            sandbox_id: id,
            exec_epoch: u64::MAX,
        };
        let _ = ryzhub_execution::call(host_socket, &destroy).await;
        if !workspaces.contains(&(org, conv)) {
            workspaces.push((org, conv));
        }
    }
    for (org_id, workspace_id) in workspaces {
        let evict = HostCommand::EvictWorkspace {
            org_id,
            workspace_id,
        };
        let _ = ryzhub_execution::call(host_socket, &evict).await;
    }
}

/// Release and drop test databases a crashed run left behind, once they are an hour old (their
/// names hold the UUIDv7 they were created with), so a run in progress is never touched.
pub async fn sweep(admin: &PgPool, root_url: &str, host_socket: &Path) {
    let Ok(names) = sqlx::query_scalar::<_, String>(
        "SELECT datname FROM pg_database WHERE datname LIKE 'ryzhub\\_test\\_%'",
    )
    .fetch_all(admin)
    .await
    else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    for name in names {
        let created = name
            .strip_prefix("ryzhub_test_")
            .and_then(|id| Uuid::parse_str(id).ok())
            .and_then(|id| id.get_timestamp())
            .map(|ts| {
                let (secs, nanos) = ts.to_unix();
                secs * 1000 + u64::from(nanos) / 1_000_000
            });
        if created.is_none_or(|at| now.saturating_sub(at) < 3_600_000) {
            continue;
        }
        if let Ok(db) = PgPool::connect(&format!("{root_url}/{name}")).await {
            release(&db, host_socket).await;
            db.close().await;
        }
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {name} WITH (FORCE)"
        )))
        .execute(admin)
        .await;
    }
}
