//! What the integration suites share: an isolated database, the API and model gateways over
//! fixture upstreams, and the execution node for suites that enable sandboxes.
#![allow(dead_code)]
pub mod upstream;

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use axum::Router;
use ryzhub_execution::{ExecSpec, GuestCommand, GuestRequest, HostCommand};
use ryzhub_proto::{CommandEnvelope, SendMessage, command_envelope::Command};
use ryzhub_server::{
    app::App,
    config::{Config, reload::Running},
    http::router,
    llm::gateway,
    store::{Conversation, Store, create_in},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{
    process::{Child, Command as Process},
    task::JoinHandle,
};
use upstream::Requests;
use uuid::Uuid;

/// The repository checkout.
pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// The execution node's socket the suites use.
pub fn host_socket() -> PathBuf {
    std::env::var_os("TEST_HOST_SOCKET")
        .map_or_else(|| workspace().join("var/execution/host.sock"), Into::into)
}

pub struct Harness {
    name: String,
    admin: PgPool,
    pub db: PgPool,
    pub app: App,
    /// The API.
    pub base: String,
    /// The model gateway, over TCP.
    pub gateway: String,
    pub app_url: String,
    pub worker_url: String,
    /// What the fixture upstreams received.
    pub requests: Requests,
    tasks: Vec<JoinHandle<()>>,
    /// The configuration, for the Owner processes.
    config: tempfile::NamedTempFile,
}

impl Harness {
    /// A harness whose configuration `configure` adjusts last. Everything is wired; suites
    /// enable the model, sandboxes and Work themselves.
    pub async fn start(configure: impl FnOnce(&mut Config)) -> anyhow::Result<Self> {
        let admin_url = std::env::var("TEST_ADMIN_URL").unwrap_or_else(|_| {
            "postgres://postgres:local-development-only@127.0.0.1:55432/postgres".into()
        });
        let admin = PgPool::connect(&admin_url).await?;
        let root = admin_url.rsplit_once('/').unwrap().0;
        sweep(&admin, root, &host_socket()).await;
        let name = format!("ryzhub_test_{}", Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await?;
        let db = PgPool::connect(&format!("{root}/{name}")).await?;
        sqlx::migrate!("../../migrations").run(&db).await?;
        let address = root.split('@').nth(1).context("test database URL")?;
        let app_url = format!("postgres://ryzhub_app:local-app-only@{address}/{name}");
        let worker_url = format!("postgres://ryzhub_worker:local-worker-only@{address}/{name}");
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let upstream = format!("http://{}", upstream_listener.local_addr()?);
        let gateway = format!("http://{}", gateway_listener.local_addr()?);
        let base = format!("http://{}", api_listener.local_addr()?);
        // The socket sandboxes reach the gateway through.
        let socket_dir = std::env::var_os("TEST_GATEWAY_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace().join("var/execution/gateway"));
        std::fs::create_dir_all(&socket_dir)?;
        let socket_path = socket_dir.join(format!(
            "llm-{}.sock",
            &Uuid::now_v7().simple().to_string()[20..]
        ));
        let unix_listener = tokio::net::UnixListener::bind(&socket_path)?;

        let mut config = Config {
            public_url: base.clone(),
            attachment_renderer: env!("CARGO_BIN_EXE_ryzhub-server").into(),
            ..Default::default()
        };
        config.llm.model = "fixture-model".into();
        config.llm.upstream_url = upstream.clone();
        config.llm.responses_url = upstream;
        config.llm.gateway_url = gateway.clone();
        config.llm.platform_key_env = "RYZHUB_TEST_UPSTREAM_KEY".into();
        config.llm.responses_key_env = "RYZHUB_TEST_UPSTREAM_KEY".into();
        config.llm.vault_key_env = "RYZHUB_TEST_VAULT_KEY".into();
        config.llm.unix_socket = Some(socket_path.display().to_string());
        config.sandbox.runtime = std::env::var("TEST_RUNTIME").unwrap_or_else(|_| "runc".into());
        config.sandbox.host_socket = host_socket().display().to_string();
        config.sandbox.image = std::fs::read_to_string(workspace().join("var/execution/image.txt"))
            .unwrap_or_default()
            .trim()
            .into();
        config.work.storage_root = format!("/tmp/{name}/primary");
        configure(&mut config);
        let mut config_file = tempfile::NamedTempFile::new()?;
        config_file.write_all(toml::to_string(&config)?.as_bytes())?;

        let store = Store::connect(&app_url).await?;
        let nats = async_nats::connect("nats://127.0.0.1:54222").await?;
        let running = Running::new(config, Some(config_file.path().into()));
        let app = App::new(store, nats, running)?;
        let mut h = Self {
            name,
            admin,
            db,
            app: app.clone(),
            base,
            gateway,
            app_url,
            worker_url,
            requests: Requests::default(),
            tasks: vec![],
            config: config_file,
        };
        h.serve(upstream_listener, upstream::router(h.requests.clone()));
        h.serve(gateway_listener, gateway::router(app.clone()));
        h.serve(unix_listener, gateway::router(app.clone()));
        h.serve(api_listener, router(app.clone()));
        // Work sandboxes keep their node lease as long as a worker role runs.
        if app.config().work.enabled {
            let worker = PgPool::connect(&h.worker_url).await?;
            h.tasks.push(tokio::spawn(async move {
                let _ = ryzhub_server::sandbox::heartbeat(app, worker).await;
            }));
        }
        Ok(h)
    }

    /// Serve `router` on `listener` until the harness closes.
    pub fn serve<L>(&mut self, listener: L, router: Router)
    where
        L: axum::serve::Listener,
        L::Addr: std::fmt::Debug,
    {
        self.tasks.push(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }));
    }

    pub async fn user(&self) -> anyhow::Result<Value> {
        let email = format!("{}@example.test", Uuid::now_v7());
        Ok(reqwest::Client::new()
            .post(format!("{}/auth/register", self.base))
            .json(&json!({"email":email,"password":"test-password"}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn context(&self, user: &Value) -> anyhow::Result<ryzhub_domain::OrgContext> {
        let (user, org) = (id(user, "user_id"), id(user, "org_id"));
        self.app.store.context(user, org).await
    }

    /// A conversation with no message yet, created as CreateRun creates one.
    pub async fn create(
        &self,
        ctx: &ryzhub_domain::OrgContext,
        title: &str,
        mode: &str,
        settings: Value,
        project: Option<Uuid>,
    ) -> anyhow::Result<Conversation> {
        let mut tx = self.app.store.tx(ctx.org_id).await?;
        let config = &self.app.config();
        let created = create_in(
            &mut tx,
            ctx,
            Uuid::now_v7(),
            title,
            mode,
            settings,
            project,
            config,
        )
        .await?
        .context("conversation id taken")?;
        tx.commit().await?;
        Ok(created)
    }

    pub async fn send(&self, user: &Value, conv: Uuid, command: Command) -> anyhow::Result<Uuid> {
        let ctx = self.context(user).await?;
        let cid = Uuid::now_v7();
        let envelope = CommandEnvelope {
            cid: cid.to_string(),
            command: Some(command),
        };
        self.app
            .store
            .accept(ctx.org_id, ctx.user_id, conv, "test-device", &envelope)
            .await?;
        Ok(cid)
    }

    pub fn owner(&self) -> anyhow::Result<Child> {
        Ok(Process::new(env!("CARGO_BIN_EXE_ryzhub-server"))
            .arg("owner")
            .env("DATABASE_URL", &self.app_url)
            .env("WORKER_DATABASE_URL", &self.worker_url)
            .env("RYZHUB_CONFIG", self.config.path())
            .env("RUST_LOG", "warn")
            .kill_on_drop(true)
            .spawn()?)
    }

    /// The conversation's latest run once it is in `state`.
    pub async fn wait_state(&self, conv: Uuid, state: &str) -> anyhow::Result<Uuid> {
        for _ in 0..1000 {
            let row: Option<(Uuid, String)> = sqlx::query_as(
                "SELECT id,state FROM runs WHERE conversation_id=$1
                 ORDER BY started_at DESC LIMIT 1",
            )
            .bind(conv)
            .fetch_optional(&self.db)
            .await?;
            if let Some((run, current)) = row {
                if current == state {
                    return Ok(run);
                }
                if current == "failed" {
                    let error: Option<String> =
                        sqlx::query_scalar("SELECT error_code FROM runs WHERE id=$1")
                            .bind(run)
                            .fetch_one(&self.db)
                            .await?;
                    let events: Vec<Value> = sqlx::query_scalar(
                        "SELECT payload FROM guest_events
                         WHERE sandbox_id=(SELECT sandbox_id FROM runs WHERE id=$1)
                           AND payload::text LIKE '%error%' ORDER BY gseq DESC LIMIT 4",
                    )
                    .bind(run)
                    .fetch_all(&self.db)
                    .await?;
                    anyhow::bail!("run failed: {error:?}; native errors: {events:?}");
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let events: Vec<String> = sqlx::query_scalar(
            "SELECT left(payload::text,400) FROM guest_events
             WHERE sandbox_id IN (SELECT id FROM sandboxes WHERE conversation_id=$1)
             ORDER BY gseq DESC LIMIT 12",
        )
        .bind(conv)
        .fetch_all(&self.db)
        .await?;
        let items: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT kind,state,left(text,80) FROM items WHERE conversation_id=$1
             ORDER BY first_seq",
        )
        .bind(conv)
        .fetch_all(&self.db)
        .await?;
        anyhow::bail!("run did not reach {state}; guest events: {events:#?}; items: {items:?}")
    }

    /// The run's pending action and the run epoch it was raised in.
    pub async fn action(&self, run: Uuid) -> anyhow::Result<(Uuid, i64)> {
        Ok(sqlx::query_as(
            "SELECT id,run_epoch FROM pending_actions WHERE run_id=$1 AND status='pending'",
        )
        .bind(run)
        .fetch_one(&self.db)
        .await?)
    }

    pub async fn close(self) -> anyhow::Result<()> {
        if self.app.config().sandbox.enabled {
            release(&self.db, Path::new(&self.app.config().sandbox.host_socket)).await;
        }
        if let Some(path) = &self.app.config().llm.unix_socket {
            let _ = std::fs::remove_file(path);
        }
        for task in self.tasks {
            task.abort();
            let _ = task.await;
        }
        self.app.store.pool.close().await;
        self.db.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await?;
        self.admin.close().await;
        Ok(())
    }
}

/// Work with Codex: two copies of every checkpoint, and sandboxes the harnesses fit in.
pub fn work(config: &mut Config) {
    config.llm.enabled = true;
    config.sandbox.enabled = true;
    config.sandbox.memory_bytes = 1024 * 1024 * 1024;
    config.work.enabled = true;
    config.work.model = "gpt-5.4".into();
    let secondary = Path::new(&config.work.storage_root).with_file_name("secondary");
    config.work.secondary_root = Some(secondary.display().to_string());
}

/// Change the configuration `app`, and every clone of it, runs with.
pub fn reconfigure(app: &App, change: impl FnOnce(&mut Config)) {
    let mut config = (*app.config()).clone();
    change(&mut config);
    app.set_config(config);
}

pub fn id(value: &Value, key: &str) -> Uuid {
    Uuid::parse_str(value[key].as_str().unwrap()).unwrap()
}

pub fn message(text: &str) -> Command {
    Command::SendMessage(SendMessage {
        text: text.into(),
        ..Default::default()
    })
}

/// Run `cmd` in the sandbox's workspace and wait for its result.
pub async fn guest_exec(
    host: &Path,
    sandbox: Uuid,
    epoch: i64,
    op: &str,
    cmd: &str,
) -> anyhow::Result<Value> {
    let call = |command| HostCommand::Guest {
        request: GuestRequest {
            sandbox_id: sandbox,
            exec_epoch: epoch as u64,
            command,
        },
    };
    let spec = ExecSpec {
        cmd: cmd.into(),
        cwd: "/workspace".into(),
        env: Default::default(),
        timeout_ms: 10000,
        background: false,
    };
    let exec = GuestCommand::Exec {
        op_id: op.into(),
        spec,
    };
    let mut result = ryzhub_execution::call(host, &call(exec)).await?;
    for _ in 0..200 {
        if result["state"] != "running" {
            return Ok(result);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let poll = GuestCommand::Poll {
            op_id: op.into(),
            wait_ms: 0,
        };
        result = ryzhub_execution::call(host, &call(poll)).await?;
    }
    anyhow::bail!("EXEC_TIMEOUT")
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
