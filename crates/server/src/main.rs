use anyhow::Context;
use ryzhub_server::{
    api::{App, router},
    config::Config,
    owner,
    store::Store,
};
use sqlx::PgPool;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let role = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    if matches!(role.as_str(), "migrate" | "maintain") {
        let pool = PgPool::connect(
            &std::env::var("MIGRATION_DATABASE_URL").context("MIGRATION_DATABASE_URL required")?,
        )
        .await?;
        if role == "migrate" {
            sqlx::migrate!("../../migrations").run(&pool).await?;
        }
        ryzhub_server::maintenance::maintain(&pool, Config::load()?.events_retention_days).await?;
        return Ok(());
    }
    anyhow::ensure!(
        matches!(role.as_str(), "all" | "gateway" | "owner" | "llm-gateway"),
        "usage: ryzhub-server [all|gateway|owner|llm-gateway|migrate|maintain]"
    );
    let config = Config::load()?;
    let store =
        Store::connect(&std::env::var("DATABASE_URL").context("DATABASE_URL required")?).await?;
    let nats = async_nats::connect(
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:54222".into()),
    )
    .await?;
    let mut tasks = tokio::task::JoinSet::new();
    if matches!(role.as_str(), "all" | "owner") {
        let worker = PgPool::connect(
            &std::env::var("WORKER_DATABASE_URL").context("WORKER_DATABASE_URL required")?,
        )
        .await?;
        tasks.spawn(ryzhub_server::projects::collector(
            App {
                store: store.clone(),
                nats: nats.clone(),
                config: config.clone(),
            },
            worker.clone(),
        ));
        if config.work.enabled {
            tasks.spawn(ryzhub_server::checkpoint::collector(
                App {
                    store: store.clone(),
                    nats: nats.clone(),
                    config: config.clone(),
                },
                worker.clone(),
            ));
        }
        tasks.spawn(owner::run(
            store.clone(),
            worker,
            nats.clone(),
            config.clone(),
        ));
    }
    if config.llm.enabled && matches!(role.as_str(), "all" | "llm-gateway") {
        anyhow::ensure!(!config.llm.model.is_empty(), "llm.model required");
        ryzhub_server::llm::identity::secret(&config.llm.signing_key_env)?;
        ryzhub_server::llm::identity::secret(&config.llm.vault_key_env)?;
        let listener = tokio::net::TcpListener::bind(&config.llm.listen).await?;
        let app = App {
            store: store.clone(),
            nats: nats.clone(),
            config: config.clone(),
        };
        if config.work.enabled {
            let heartbeat_app = app.clone();
            let worker = PgPool::connect(
                &std::env::var("WORKER_DATABASE_URL").context("WORKER_DATABASE_URL required")?,
            )
            .await?;
            tasks.spawn(ryzhub_server::checkpoint::heartbeat(heartbeat_app, worker));
        }
        if let Some(path) = &config.llm.unix_socket {
            use std::os::unix::fs::{FileTypeExt, PermissionsExt};
            if let Some(parent) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(parent)?;
            }
            if std::path::Path::new(path).exists() {
                anyhow::ensure!(
                    std::fs::symlink_metadata(path)?.file_type().is_socket(),
                    "gateway path is not a socket"
                );
                anyhow::ensure!(
                    tokio::net::UnixStream::connect(path).await.is_err(),
                    "gateway socket already in use"
                );
                std::fs::remove_file(path)?;
            }
            let listener = tokio::net::UnixListener::bind(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            let app = app.clone();
            tasks.spawn(async move {
                axum::serve(listener, ryzhub_server::llm::gateway::router(app))
                    .await
                    .map_err(Into::into)
            });
        }
        tracing::info!(address=%config.llm.listen,"LLM gateway listening");
        tasks.spawn(async move {
            axum::serve(listener, ryzhub_server::llm::gateway::router(app))
                .await
                .map_err(Into::into)
        });
    }
    if matches!(role.as_str(), "all" | "gateway") {
        let listener = tokio::net::TcpListener::bind(&config.listen).await?;
        tracing::info!(address=%config.listen,"RyzHub gateway listening");
        tasks.spawn(async move {
            axum::serve(
                listener,
                router(App {
                    store,
                    nats,
                    config,
                }),
            )
            .await
            .map_err(Into::into)
        });
    }
    tokio::select! {
        _=tokio::signal::ctrl_c()=>{},
        result=tasks.join_next()=>{if let Some(result)=result {result??;}},
    }
    tasks.abort_all();
    Ok(())
}
