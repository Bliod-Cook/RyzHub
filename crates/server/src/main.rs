//! `ryzhub-server <role>`: all | gateway | owner | llm-gateway | migrate | maintain [--loop] |
//! reindex | migrate-secrets.
use anyhow::{Context, ensure};
use ryzhub_server::{
    accounts,
    app::App,
    attachments,
    components::{self, Components},
    config::{
        Config,
        reload::{self, Running},
    },
    conversation_commands, lifecycle, memory, projects, push, retention, sandbox, search,
    store::Store,
    workspace,
};
use sqlx::PgPool;
use std::{future::Future, time::Duration};
use tokio::{
    signal::unix::{SignalKind, signal},
    task::JoinSet,
};

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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let stop = runtime.block_on(run(args))?;
    drop(runtime);
    if let Stop::Restart = stop {
        std::process::exit(components::EXIT_RESTART);
    }
    Ok(())
}

async fn run(args: Vec<String>) -> anyhow::Result<Stop> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let role = args.get(1).map_or("all", String::as_str);
    let repeat = args.iter().any(|a| a == "--loop");
    match role {
        "maintain" if repeat => until_stopped(maintain(role, true)).await,
        "migrate" | "maintain" | "reindex" | "migrate-secrets" => {
            maintain(role, repeat).await.map(|()| Stop::Done)
        }
        "all" | "gateway" | "owner" | "llm-gateway" => until_stopped(serve(role)).await,
        _ => anyhow::bail!(
            "usage: ryzhub-server [all|gateway|owner|llm-gateway|migrate|maintain [--loop]|reindex|migrate-secrets]"
        ),
    }
}

/// Why the process stops.
enum Stop {
    Done,
    /// An admin asked for a restart.
    Restart,
}

/// Runs `task` until it ends or the process is asked to stop: Ctrl-C, SIGTERM from
/// `docker stop` and service managers, or an admin's restart. Connections are dropped;
/// clients reconnect, and conversation leases move to another owner within seconds.
async fn until_stopped(task: impl Future<Output = anyhow::Result<()>>) -> anyhow::Result<Stop> {
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = task => result.map(|()| Stop::Done),
        result = tokio::signal::ctrl_c() => result.map(|()| Stop::Done).map_err(Into::into),
        _ = terminate.recv() => Ok(Stop::Done),
        () = components::restart_requested() => Ok(Stop::Restart),
    }
}

async fn serve(role: &str) -> anyhow::Result<()> {
    let running = Running::open()?;
    let store = Store::connect(&env("DATABASE_URL")?).await?;
    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:54222".into());
    let nats = async_nats::connect(nats_url).await?;
    let mut app = App::new(store, nats, running)?;
    app.components = Components::new(role);
    let watched = app.running.clone();
    (app.components).supervise("config-reload", true, move || {
        reload::watch(watched.clone())
    });
    let owner = matches!(role, "all" | "owner");
    let model_gateway = app.config().llm.enabled && matches!(role, "all" | "llm-gateway");
    let worker = if owner || (model_gateway && app.config().work.enabled) {
        Some(PgPool::connect(&env("WORKER_DATABASE_URL")?).await?)
    } else {
        None
    };
    // Servers are required: if one stops, the process exits. Background loops are supervised.
    let mut servers = JoinSet::new();
    if let (true, Some(worker)) = (owner, &worker) {
        supervise("owner", &app, worker, ryzhub_server::owner::run);
        supervise("project-collector", &app, worker, projects::collector);
        if app.config().search.enabled {
            supervise("search-index", &app, worker, search::worker);
        }
        supervise("memory-collector", &app, worker, memory::collector);
        supervise(
            "conversation-collector",
            &app,
            worker,
            conversation_commands::collector,
        );
        supervise(
            "attachment-collector",
            &app,
            worker,
            attachments::gc::collector,
        );
        supervise("push", &app, worker, push::worker);
        supervise("conversation-lifecycle", &app, worker, lifecycle::collector);
        supervise("retention", &app, worker, retention::collector);
        supervise("account-deletion", &app, worker, accounts::collector);
        if app.config().work.enabled || sandbox::chat_checkpoints(&app) {
            supervise("checkpoint-collector", &app, worker, workspace::collector);
        }
    }
    if let (true, true, Some(worker)) = (owner, app.config().sandbox.enabled, &worker) {
        supervise("node-reclamation", &app, worker, sandbox::reclaim);
    }
    if app.config().work.enabled
        && let Some(worker) = &worker
    {
        // Both roles renew node leases, so a rolling restart of either keeps them alive.
        supervise("sandbox-heartbeat", &app, worker, sandbox::heartbeat);
    }
    if model_gateway {
        start_model_gateway(&app, &mut servers).await?;
    }
    if matches!(role, "all" | "gateway") {
        let listener = tokio::net::TcpListener::bind(&app.config().listen).await?;
        tracing::info!(address = %app.config().listen, "RyzHub gateway listening");
        let router = ryzhub_server::http::router(app.clone());
        servers.spawn(async move { axum::serve(listener, router).await.map_err(Into::into) });
        app.components.listening("api");
        if app.config().plugins.enabled {
            let plugins = &app.config().plugins;
            let listener = tokio::net::TcpListener::bind(&plugins.content_listen).await?;
            tracing::info!(address = %plugins.content_listen, "plugin content origin listening");
            let router = ryzhub_server::plugins::apps::content_router(app.clone());
            servers.spawn(async move { axum::serve(listener, router).await.map_err(Into::into) });
            app.components.listening("plugin-content");
        }
    }
    match servers.join_next().await {
        Some(result) => result?,
        None => std::future::pending().await,
    }
}

fn env(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| format!("{name} required"))
}

/// Run a background loop as long as the process runs (see [`Components::supervise`]). The
/// Owner and what keeps sandboxes alive cannot be restarted on their own.
fn supervise<F, Fut>(name: &'static str, app: &App, worker: &PgPool, task: F)
where
    F: Fn(App, PgPool) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let (owned, worker) = (app.clone(), worker.clone());
    let restartable = !matches!(name, "owner" | "node-reclamation" | "sandbox-heartbeat");
    app.components.supervise(name, restartable, move || {
        task(owned.clone(), worker.clone())
    });
}

async fn start_model_gateway(
    app: &App,
    servers: &mut JoinSet<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let llm = &app.config().llm;
    ryzhub_server::vault::secret(&llm.vault_key_env)?;
    let listener = tokio::net::TcpListener::bind(&llm.listen).await?;
    if let Some(path) = &llm.unix_socket {
        let socket = bind_unix_socket(path).await?;
        let router = ryzhub_server::llm::gateway::router(app.clone());
        servers.spawn(async move { axum::serve(socket, router).await.map_err(Into::into) });
        app.components.listening("llm-gateway-socket");
    }
    tracing::info!(address = %llm.listen, "LLM gateway listening");
    let router = ryzhub_server::llm::gateway::router(app.clone());
    servers.spawn(async move { axum::serve(listener, router).await.map_err(Into::into) });
    app.components.listening("llm-gateway");
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
    let mut config = Config::load()?;
    if role == "migrate-secrets" {
        let rewrapped = ryzhub_server::vault::migrate(&pool, &config.llm).await?;
        tracing::info!(rewrapped, "secrets migrated");
        return Ok(());
    }
    if role == "reindex" {
        let queued = search::reindex(&pool).await?;
        tracing::info!(queued, "search backfill queued");
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
        // Retention changed meanwhile applies from the next round.
        match Config::load() {
            Ok(fresh) => config = fresh,
            Err(error) => tracing::error!(
                error = format!("{error:#}"),
                "configuration not reloaded; maintaining with the previous one"
            ),
        }
    }
}
