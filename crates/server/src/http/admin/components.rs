//! `/admin/components` and `/admin/process`: what runs in the process that answers, and
//! restarting it or one of its loops.
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::time::Duration;

use super::{admin, audit};
use crate::{
    app::App,
    components,
    config::reload::version,
    error::{ApiResult, Code},
};

pub fn routes() -> Router<App> {
    Router::new()
        .route("/admin/components", get(list))
        .route("/admin/components/{name}/restart", post(restart))
        .route("/admin/process/restart", post(restart_process))
}

/// `GET /admin/components`: this process, its loops and listeners, and the execution nodes.
async fn list(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    admin(&app, &headers).await?;
    let nodes: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('name',n.name,'state',n.state,
           'lease_until_ms',(extract(epoch FROM n.lease_until)*1000)::bigint,
           'live',coalesce(l.live,0),'capacity',n.capacity)
         FROM execution_nodes n LEFT JOIN node_load() l ON l.node=n.name ORDER BY n.name",
    )
    .fetch_all(&app.store.pool)
    .await?;
    let config_version = (app.running.path.as_ref())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| version(&text));
    let host = nix::unistd::gethostname()?.to_string_lossy().into_owned();
    Ok(Json(json!({
        "process": {
            "role": app.components.role,
            "host": host,
            "pid": std::process::id(),
            "started_at_ms": app.components.started_at.timestamp_millis(),
            "version": env!("CARGO_PKG_VERSION"),
            "config_version": config_version,
        },
        "components": app.components.list(),
        "nodes": nodes,
    })))
}

/// `POST /admin/components/{name}/restart`: stop the loop at its next await point and start it
/// again. What it had claimed is taken up again once its lease runs out.
async fn restart(
    State(app): State<App>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let auth = admin(&app, &headers).await?;
    require!(app.components.restart(&name), Code::InvalidInput);
    let mut tx = app.store.pool.begin().await?;
    audit(&mut tx, &auth, "admin.component.restart", &name, json!({})).await?;
    tx.commit().await?;
    Ok(StatusCode::ACCEPTED)
}

/// `POST /admin/process/restart`: answer, then stop as on SIGTERM with the exit status that
/// has the process started again.
async fn restart_process(State(app): State<App>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let auth = admin(&app, &headers).await?;
    let mut tx = app.store.pool.begin().await?;
    let role = app.components.role.clone();
    audit(&mut tx, &auth, "admin.process.restart", &role, json!({})).await?;
    tx.commit().await?;
    tokio::spawn(async {
        // Time for the answer to leave.
        tokio::time::sleep(Duration::from_millis(200)).await;
        components::request_restart();
    });
    Ok(StatusCode::ACCEPTED)
}
