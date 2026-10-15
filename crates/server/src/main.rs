//! `ryzhub-server <role>`: all | gateway | owner | llm-gateway | migrate | maintain [--loop] |
//! reindex | migrate-secrets.
use anyhow::{Context, ensure};
use ryzhub_server::{app::App, config::Config, store::Store};
use sqlx::PgPool;
use std::{future::Future, time::Duration};
use tokio::task::JoinSet;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Decodes one uploaded image for the server, before any runtime or thread exists.
    if args.get(1).is_some_and(|role| role == "render-image") {
        return ryzhub_execution::picture::child_main(&args[2..]);
    }
    // Reads one plugin package, likewise in a process of its own.
    if args.get(1).is_some_and(|role| role == "parse-plugin") {
        return ryzhub_server::plugins::package::child_main();
    }
    // Reads one document's text, likewise.
    if args.get(1).is_some_and(|role| role == "extract-text") {
        return ryzhub_server::extract::child_main(&args[2..]);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args))
}

async fn run(args: Vec<String>) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let role = args.get(1).map_or("all", String::as_str);
    if matches!(role, "migrate" | "maintain" | "reindex" | "migrate-secrets") {
        return maintain(role, args.iter().any(|a| a == "--loop")).await;
    }
    ensure!(
        matches!(role, "all" | "gateway" | "owner" | "llm-gateway"),
        "usage: ryzhub-server [all|gateway|owner|llm-gateway|migrate|maintain [--loop]|reindex|migrate-secrets]"
    );
    let config = Config::load()?;
    let store = Store::connect(&env("DATABASE_URL")?).await?;
    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:54222".into());
    let nats = async_nats::connect(nats_url).await?;
    let app = App::new(store, nats, config)?;
    let owner = matches!(role, "all" | "owner");
    let model_gateway = app.config.llm.enabled && matches!(role, "all" | "llm-gateway");
    let worker = if owner || (model_gateway && app.config.work.enabled) {
        Some(PgPool::connect(&env("WORKER_DATABASE_URL")?).await?)
    } else {
        None
    };
    // Servers are required: if one stops, the process exits. Background loops are supervised.
    let mut servers = JoinSet::new();
    if let (true, Some(worker)) = (owner, &worker) {
        supervise("owner", {
            let (app, worker) = (app.clone(), worker.clone());
            move || ryzhub_server::owner::run(app.clone(), worker.clone())
        });
        supervise("project collector", {
            let (app, worker) = (app.clone(), worker.clone());
            move || ryzhub_server::projects::collector(app.clone(), worker.clone())
        });
        if app.config.search.enabled {
            supervise("search index", {
                let (app, worker) = (app.clone(), worker.clone());
                move || ryzhub_server::search::worker(app.clone(), worker.clone())
            });
        }
        supervise("memory and conversation collector", {
            let (app, worker) = (app.clone(), worker.clone());
            move || ryzhub_server::memory::collector(app.clone(), worker.clone())
        });
        supervise("attachment collector", {
            let (app, worker) = (app.clone(), worker.clone());
            move || ryzhub_server::attachments::gc::collector(app.clone(), worker.clone())
        });
        if !ryzhub_server::jobs::handlers().is_empty() {
            supervise("jobs", {
                let (app, worker) = (app.clone(), worker.clone());
                move || {
                    ryzhub_server::jobs::worker(
                        app.clone(),
                        worker.clone(),
                        ryzhub_server::jobs::handlers(),
                    )
                }
            });
        }
        supervise("conversation lifecycle", {
            let (app, worker) = (app.clone(), worker.clone());
            move || ryzhub_server::lifecycle::collector(app.clone(), worker.clone())
        });
        supervise("retention", {
            let (app, worker) = (app.clone(), worker.clone());
            move || ryzhub_server::retention::collector(app.clone(), worker.clone())
        });
        if app.config.work.enabled || ryzhub_server::sandbox::chat_checkpoints(&app) {
            supervise("checkpoint collector", {
                let (app, worker) = (app.clone(), worker.clone());
                move || ryzhub_server::workspace::collector(app.clone(), worker.clone())
            });
        }
    }
    if let (true, true, Some(worker)) = (owner, app.config.sandbox.enabled, &worker) {
        supervise("node reclamation", {
            let (app, worker) = (app.clone(), worker.clone());
            move || ryzhub_server::sandbox::reclaim(app.clone(), worker.clone())
        });
    }
    if app.config.work.enabled
        && let Some(worker) = &worker
    {
        // Both roles renew node leases, so a rolling restart of either keeps them alive.
        supervise("sandbox heartbeat", {
            let (app, worker) = (app.clone(), worker.clone());
            move || ryzhub_server::sandbox::heartbeat(app.clone(), worker.clone())
        });
    }
    if model_gateway {
        start_model_gateway(&app, &mut servers).await?;
    }
    if matches!(role, "all" | "gateway") {
        let listener = tokio::net::TcpListener::bind(&app.config.listen).await?;
        tracing::info!(address = %app.config.listen, "RyzHub gateway listening");
        let router = ryzhub_server::http::router(app.clone());
        servers.spawn(async move { axum::serve(listener, router).await.map_err(Into::into) });
        if app.config.plugins.enabled {
            let plugins = &app.config.plugins;
            let listener = tokio::net::TcpListener::bind(&plugins.content_listen).await?;
            tracing::info!(address = %plugins.content_listen, "plugin content origin listening");
            let router = ryzhub_server::plugins::apps::content_router(app.clone());
            servers.spawn(async move { axum::serve(listener, router).await.map_err(Into::into) });
        }
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => Ok(()),
        Some(result) = servers.join_next() => result?,
        () = std::future::pending(), if servers.is_empty() => Ok(()),
    }
}

fn env(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| format!("{name} required"))
}

/// Run a background loop forever, restarting it with backoff when it fails.
fn supervise<F, Fut>(name: &'static str, task: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send,
{
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(500);
        loop {
            let started = tokio::time::Instant::now();
            match task().await {
                Ok(()) => tracing::warn!(task = name, "background task returned; restarting"),
                Err(error) => tracing::error!(
                    task = name,
                    error = format!("{error:#}"),
                    "background task failed; restarting"
                ),
            }
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_millis(500);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
}

async fn start_model_gateway(
    app: &App,
    servers: &mut JoinSet<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let llm = &app.config.llm;
    ensure!(!llm.model.is_empty(), "llm.model required");
    ryzhub_server::vault::secret(&llm.vault_key_env)?;
    let listener = tokio::net::TcpListener::bind(&llm.listen).await?;
    if let Some(path) = &llm.unix_socket {
        let socket = bind_unix_socket(path).await?;
        let router = ryzhub_server::llm::gateway::router(app.clone());
        servers.spawn(async move { axum::serve(socket, router).await.map_err(Into::into) });
    }
    tracing::info!(address = %llm.listen, "LLM gateway listening");
    let router = ryzhub_server::llm::gateway::router(app.clone());
    servers.spawn(async move { axum::serve(listener, router).await.map_err(Into::into) });
    Ok(())
}

/// Bind the socket that is bind-mounted into sandboxes, replacing a stale one.
async fn bind_unix_socket(path: &str) -> anyhow::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let path = std::path::Path::new(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        ensure!(
            std::fs::symlink_metadata(path)?.file_type().is_socket(),
            "gateway path is not a socket"
        );
        ensure!(
            tokio::net::UnixStream::connect(path).await.is_err(),
            "gateway socket already in use"
        );
        std::fs::remove_file(path)?;
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// `migrate` applies migrations then maintains partitions; `maintain --loop` repeats daily.
/// `migrate-secrets` needs the vault keys in its environment.
async fn maintain(role: &str, repeat: bool) -> anyhow::Result<()> {
    let pool = PgPool::connect(&env("MIGRATION_DATABASE_URL")?).await?;
    if role == "migrate" {
        sqlx::migrate!("../../migrations").run(&pool).await?;
    }
    let config = Config::load()?;
    if role == "migrate-secrets" {
        let (converted, rewrapped) = ryzhub_server::vault::migrate(&pool, &config.llm).await?;
        tracing::info!(converted, rewrapped, "secrets migrated");
        return Ok(());
    }
    if role == "reindex" {
        sqlx::query("INSERT INTO search_jobs(org_id,source_id,source_kind,source_version,op,available_at,priority)
            SELECT org_id,id,doc_kind,source_version,'upsert',now(),-10 FROM search_sources WHERE NOT deleted
            ON CONFLICT(org_id,source_id,source_version) DO UPDATE SET state='queued',attempts=0,
                claim_token=NULL,lease_until=NULL,available_at=excluded.available_at,priority=-10").execute(&pool).await?;
        tracing::info!("search backfill queued");
        return Ok(());
    }
    loop {
        match ryzhub_server::maintenance::maintain(&pool, &config).await {
            Ok(()) => tracing::info!("maintenance complete"),
            Err(error) if repeat => {
                tracing::error!(error = format!("{error:#}"), "maintenance failed")
            }
            Err(error) => return Err(error),
        }
        if !repeat {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
    }
}
