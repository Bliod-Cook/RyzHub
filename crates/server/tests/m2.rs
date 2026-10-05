//! Real PostgreSQL, NATS and containerd; only the paid model upstream is a protocol fixture.
use anyhow::{Context, ensure};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::HeaderMap,
    routing::post,
};
use ryzhub_proto::{command_envelope::Command, *};
use ryzhub_server::{
    app::App,
    config::Config,
    http::router,
    llm::{gateway, keys},
    owner,
    store::Store,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    process::{Child, Command as Process},
    task::JoinHandle,
};
use uuid::Uuid;

type Requests = Arc<Mutex<Vec<(Value, HeaderMap)>>>;
async fn upstream(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    requests.lock().unwrap().push((body.clone(), headers));
    let last = body["messages"].as_array().unwrap().last().unwrap()["content"].clone();
    let prompt = last.as_str().unwrap_or("");
    let result = last
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"));
    let mut content = vec![
        json!({"type":"thinking","thinking":"opaque reasoning","signature":"signed-block"}),
        json!({"type":"redacted_thinking","data":"opaque-data"}),
    ];
    if !result && body.get("tools").is_some() && prompt.contains("exec") {
        let command = if prompt.contains("proxyexec") {
            r#"sleep 2; wget -qO- --header 'Content-Type: application/json' --post-data '{"model":"fixture-model","max_tokens":32,"stream":true,"messages":[{"role":"user","content":"guest local proxy"}]}' http://127.0.0.1:8090/v1/messages"#
        } else {
            "printf once >> marker; sleep 1; printf '真实沙箱结果'"
        };
        content.push(json!({"type":"text","text":"准备执行工具"}));
        content.push(json!({"type":"tool_use","id":"call_exec","name":"exec","input":{"cmd":command,"timeout_ms":10000}}));
    } else if !result && body.get("tools").is_some() && prompt.contains("question") {
        content.push(json!({"type":"tool_use","id":"call_question","name":"ask_user","input":{"question":"选择一种颜色"}}));
    } else {
        content.push(json!({"type":"text","text":if result{"工具结果已用于回答"}else if body.get("tools").is_none(){"只读旁问完成"}else{"本地协议联调：你好 Rust"}}));
    }
    let tool = content.iter().any(|b| b["type"] == "tool_use");
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"fixture-message","type":"message","role":"assistant","content":[],"model":"fixture-model","usage":{"input_tokens":25,"output_tokens":1,"cache_read_input_tokens":12}}}),
        json!({"type":"ping"}),
    ];
    for (index, block) in content.iter().enumerate() {
        let mut start = block.clone();
        if block["type"] == "text" {
            start["text"] = json!("");
        }
        events.push(json!({"type":"content_block_start","index":index,"content_block":start}));
        if block["type"] == "text" {
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":block["text"]}}));
        }
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":if tool{"tool_use"}else{"end_turn"}},"usage":{"output_tokens":4}}));
    events.push(json!({"type":"message_delta","delta":{},"usage":{"output_tokens":8}}));
    events.push(json!({"type":"message_stop"}));
    let delay = if prompt.contains("slow") { 100 } else { 5 };
    if prompt.contains("interrupt") {
        events.pop();
    }
    let stream = futures_util::stream::unfold(
        (events.into_iter(), delay),
        |(mut events, delay)| async move {
            let event = events.next()?;
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Some((
                Ok::<_, std::io::Error>(Bytes::from(format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                ))),
                (events, delay),
            ))
        },
    );
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}
struct Harness {
    name: String,
    admin: PgPool,
    db: PgPool,
    app: App,
    base: String,
    gateway: String,
    app_url: String,
    worker_url: String,
    requests: Requests,
    tasks: Vec<JoinHandle<()>>,
    config: tempfile::NamedTempFile,
}
impl Harness {
    async fn new() -> anyhow::Result<Self> {
        let admin_url = std::env::var("TEST_ADMIN_URL").unwrap_or_else(|_| {
            "postgres://postgres:local-development-only@127.0.0.1:55432/postgres".into()
        });
        let admin = PgPool::connect(&admin_url).await?;
        let name = format!("ryzhub_test_{}", Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await?;
        let root = admin_url.rsplit_once('/').unwrap().0;
        let db = PgPool::connect(&format!("{root}/{name}")).await?;
        sqlx::migrate!("../../migrations").run(&db).await?;
        let address = root.split('@').nth(1).context("test database URL")?;
        let app_url = format!("postgres://ryzhub_app:local-app-only@{address}/{name}");
        let worker_url = format!("postgres://ryzhub_worker:local-worker-only@{address}/{name}");
        let store = Store::connect(&app_url).await?;
        let nats = async_nats::connect("nats://127.0.0.1:54222").await?;
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let mut config = Config::default();
        config.llm.enabled = true;
        config.work.enabled = true;
        config.work.mcp_tools = vec![ryzhub_server::mcp::Tool {
            name: "write_note".into(),
            description: "Local test tool".into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
            url: format!("http://{}/mcp", upstream_listener.local_addr()?),
            remote_name: "write_note".into(),
        }];
        config.work.model = "gpt-5.4".into();
        config.work.storage_root = format!("/tmp/{name}/primary");
        config.work.secondary_root = Some(format!("/tmp/{name}/secondary"));
        config.llm.responses_key_env = "RYZHUB_TEST_UPSTREAM_KEY".into();
        config.llm.responses_url = format!("http://{}", upstream_listener.local_addr()?);
        config.llm.model = "fixture-model".into();
        config.llm.upstream_url = format!("http://{}", upstream_listener.local_addr()?);
        config.llm.gateway_url = format!("http://{}", gateway_listener.local_addr()?);
        config.llm.platform_key_env = "RYZHUB_TEST_UPSTREAM_KEY".into();
        config.llm.vault_key_env = "RYZHUB_TEST_VAULT_KEY".into();
        config.sandbox.enabled = true;
        config.sandbox.memory_bytes = 1024 * 1024 * 1024;
        config.sandbox.runtime = std::env::var("TEST_RUNTIME").unwrap_or_else(|_| "runc".into());
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?;
        config.sandbox.host_socket = std::env::var("TEST_HOST_SOCKET").unwrap_or_else(|_| {
            workspace
                .join("var/execution/host.sock")
                .display()
                .to_string()
        });
        config.sandbox.image = std::fs::read_to_string(workspace.join("var/execution/image.txt"))
            .unwrap_or_default()
            .trim()
            .into();
        let socket_dir = std::env::var_os("TEST_GATEWAY_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| workspace.join("var/execution/gateway"));
        std::fs::create_dir_all(&socket_dir)?;
        let socket_path = socket_dir.join(format!(
            "llm-{}.sock",
            &Uuid::now_v7().simple().to_string()[20..]
        ));
        let unix_listener = tokio::net::UnixListener::bind(&socket_path)?;
        config.llm.unix_socket = Some(socket_path.display().to_string());
        let mut config_file = tempfile::NamedTempFile::new()?;
        config_file.write_all(toml::to_string(&config)?.as_bytes())?;
        let gateway = config.llm.gateway_url.clone();
        let base = format!("http://{}", api_listener.local_addr()?);
        let app = App::new(store, nats, config)?;
        let requests = Requests::default();
        let capture = requests.clone();
        let a = app.clone();
        let b = app.clone();
        let c = app.clone();
        let mut tasks = vec![
            tokio::spawn(async move {
                axum::serve(unix_listener, gateway::router(c))
                    .await
                    .unwrap();
            }),
            tokio::spawn(async move {
                axum::serve(
                    upstream_listener,
                    Router::new()
                        .route("/v1/messages", post(upstream))
                        .route("/v1/responses", post(responses))
                        .route("/mcp", post(mcp_upstream))
                        .with_state(capture),
                )
                .await
                .unwrap();
            }),
            tokio::spawn(async move {
                axum::serve(gateway_listener, gateway::router(a))
                    .await
                    .unwrap();
            }),
            tokio::spawn(async move {
                axum::serve(api_listener, router(b)).await.unwrap();
            }),
        ];
        let heartbeat_app = app.clone();
        let heartbeat_db = PgPool::connect(&worker_url).await?;
        tasks.push(tokio::spawn(async move {
            let _ = ryzhub_server::sandbox::heartbeat(heartbeat_app, heartbeat_db).await;
        }));
        Ok(Self {
            name,
            admin,
            db,
            app,
            base,
            gateway,
            app_url,
            worker_url,
            requests,
            tasks,
            config: config_file,
        })
    }
    async fn user(&self) -> anyhow::Result<Value> {
        Ok(reqwest::Client::new().post(format!("{}/auth/register",self.base)).json(&json!({"email":format!("{}@example.test",Uuid::now_v7()),"password":"test-password"})).send().await?.error_for_status()?.json().await?)
    }
    async fn conversation(&self, user: &Value) -> anyhow::Result<Uuid> {
        let ctx = self
            .app
            .store
            .context(id(user, "user_id"), id(user, "org_id"))
            .await?;
        Ok(self
            .app
            .store
            .create_conversation(&ctx, "M2", "work")
            .await?
            .id)
    }
    async fn send(&self, user: &Value, conv: Uuid, command: Command) -> anyhow::Result<Uuid> {
        let ctx = self
            .app
            .store
            .context(id(user, "user_id"), id(user, "org_id"))
            .await?;
        let cid = Uuid::now_v7();
        self.app
            .store
            .accept(
                &ctx,
                conv,
                "m1-test",
                &CommandEnvelope {
                    cid: cid.to_string(),
                    command: Some(command),
                },
            )
            .await?;
        Ok(cid)
    }
    fn owner(&self) -> anyhow::Result<Child> {
        Ok(Process::new(env!("CARGO_BIN_EXE_ryzhub-server"))
            .arg("owner")
            .env("DATABASE_URL", &self.app_url)
            .env("WORKER_DATABASE_URL", &self.worker_url)
            .env("RYZHUB_CONFIG", self.config.path())
            .env("RUST_LOG", "warn")
            .kill_on_drop(true)
            .spawn()?)
    }
    async fn wait_state(&self, conv: Uuid, state: &str) -> anyhow::Result<Uuid> {
        for _ in 0..1000 {
            let row:Option<(Uuid,String)>=sqlx::query_as("SELECT id,state FROM runs WHERE conversation_id=$1 ORDER BY started_at DESC LIMIT 1").bind(conv).fetch_optional(&self.db).await?;
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
                    anyhow::bail!("run failed: {error:?}");
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("run did not reach {state}")
    }
    async fn action(&self, run: Uuid) -> anyhow::Result<(Uuid, i64)> {
        Ok(sqlx::query_as(
            "SELECT id,run_epoch FROM pending_actions WHERE run_id=$1 AND status='pending'",
        )
        .bind(run)
        .fetch_one(&self.db)
        .await?)
    }
    async fn close(self) -> anyhow::Result<()> {
        let sandboxes: Vec<(Uuid, i64)> = sqlx::query_as("SELECT id,exec_epoch FROM sandboxes")
            .fetch_all(&self.db)
            .await?;
        for (id, epoch) in sandboxes {
            let _ = ryzhub_execution::call(
                std::path::Path::new(&self.app.config.sandbox.host_socket),
                &ryzhub_execution::HostCommand::Destroy {
                    sandbox_id: id,
                    exec_epoch: (epoch + 100) as u64,
                },
            )
            .await;
        }
        if let Some(path) = &self.app.config.llm.unix_socket {
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
fn id(value: &Value, key: &str) -> Uuid {
    Uuid::parse_str(value[key].as_str().unwrap()).unwrap()
}
fn message(text: &str) -> Command {
    Command::SendMessage(SendMessage {
        text: text.into(),
        ..Default::default()
    })
}

async fn responses(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    requests.lock().unwrap().push((body.clone(), headers));
    let rid = format!("resp_{}", Uuid::now_v7());
    let mid = format!("msg_{}", Uuid::now_v7());
    let text = "Codex 本地协议联调成功：你好 Rust。";
    let mut item = json!({"id":mid,"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]});
    let tool_result = body["input"]
        .as_array()
        .is_some_and(|v| v.iter().any(|i| i["type"] == "function_call_output"));
    let exec = body["input"].to_string().contains("native-exec") && !tool_result;
    if exec {
        let names = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect::<Vec<_>>();
        let (name, args) = if names.contains(&"exec_command") {
            (
                "exec_command",
                json!({"cmd":"printf native-once >> /workspace/approval-marker; cat /workspace/approval-marker","max_output_tokens":100}),
            )
        } else {
            (
                "shell_command",
                json!({"command":"printf native-once >> /workspace/approval-marker; cat /workspace/approval-marker","workdir":"/workspace"}),
            )
        };
        item = json!({"id":mid,"type":"function_call","call_id":"fixture-native-command","name":name,"arguments":args.to_string(),"status":"completed"});
    }
    let response = json!({"id":rid,"object":"response","created_at":0,"model":body["model"],"status":"completed","output":[item],"usage":{"input_tokens":31,"output_tokens":9,"total_tokens":40,"input_tokens_details":{"cached_tokens":7}}});
    let delay = if body.to_string().contains("slow") {
        1000
    } else {
        50
    };
    let mut events = vec![
        json!({"type":"response.created","response":{"id":rid,"object":"response","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":mid,"type":"message","role":"assistant","status":"in_progress","content":[]}}),
    ];
    for delta in if exec {
        vec![]
    } else {
        vec!["Codex 本地协议联调成功：", "你好 Rust。"]
    } {
        events.push(json!({"type":"response.output_text.delta","item_id":mid,"output_index":0,"content_index":0,"delta":delta}));
    }
    if exec {
        events[1] = json!({"type":"response.output_item.added","output_index":0,"item":item});
    }
    events.push(json!({"type":"response.output_item.done","output_index":0,"item":item}));
    events.push(json!({"type":"response.completed","response":response}));
    let stream =
        futures_util::stream::unfold((events.into_iter(), delay), |(mut e, delay)| async move {
            let event = e.next()?;
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Some((
                Ok::<_, std::io::Error>(Bytes::from(format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                ))),
                (e, delay),
            ))
        });
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS/containerd and actual Codex binary"]
async fn actual_codex_native_turn_and_checkpoint_restore() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    let mut owner = h.owner()?;
    reqwest::Client::new().post(format!("{}/orgs/{org}/credentials",h.base)).bearer_auth(user["token"].as_str().unwrap()).json(&json!({"name":"Anthropic isolation fixture","provider":"anthropic","api_key":"must-not-reach-openai"})).send().await?.error_for_status()?;
    h.send(&user, conv, message("hello codex")).await?;
    let run = h.wait_state(conv, "completed").await?;
    let text:String=sqlx::query_scalar("SELECT text FROM items WHERE run_id=$1 AND kind='assistant' ORDER BY last_seq DESC LIMIT 1").bind(run).fetch_one(&h.db).await?;
    assert!(text.contains("Codex"), "{text}");
    let usage:(String,i64,i64,bool)=sqlx::query_as("SELECT provider,input_tokens,output_tokens,estimated FROM usage_records WHERE run_id=$1 ORDER BY created_at DESC LIMIT 1").bind(run).fetch_one(&h.db).await?;
    assert_eq!(usage, ("openai".into(), 31, 9, false));
    let key = h
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|(b, _)| b.get("model").is_some())
        .context("NO_MODEL_REQUEST")?
        .1["authorization"]
        .to_str()?
        .to_owned();
    assert_eq!(key, "Bearer local-fixture-key");
    let sandbox: Uuid =
        sqlx::query_scalar("SELECT id FROM sandboxes WHERE run_id=$1 AND persistent")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
    let epoch: i64 = sqlx::query_scalar("SELECT exec_epoch FROM sandboxes WHERE id=$1")
        .bind(sandbox)
        .fetch_one(&h.db)
        .await?;
    wait_workspace_idle(&h, conv).await?;
    let host = std::path::Path::new(&h.app.config.sandbox.host_socket);
    use ryzhub_execution::{ExecSpec, GuestCommand, GuestRequest, HostCommand};
    let exec = |cmd: &str, op: &str| HostCommand::Guest {
        request: GuestRequest {
            sandbox_id: sandbox,
            exec_epoch: epoch as u64,
            command: GuestCommand::Exec {
                op_id: op.into(),
                spec: ExecSpec {
                    cmd: cmd.into(),
                    cwd: "/workspace".into(),
                    env: Default::default(),
                    timeout_ms: 10000,
                    background: false,
                    pty: false,
                },
            },
        },
    };
    ryzhub_execution::call(host,&exec("printf checkpoint > /workspace/untracked; chmod 751 /workspace/untracked; mv /etc/periodic /etc/renamed-periodic; chmod 700 /etc/renamed-periodic", "prepare-checkpoint")).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    async fn job(
        h: &Harness,
        user: &Value,
        org: Uuid,
        conv: Uuid,
        kind: &str,
    ) -> anyhow::Result<()> {
        let mut accepted = None;
        for _ in 0..300 {
            let response = reqwest::Client::new()
                .post(format!(
                    "{}/orgs/{org}/conversations/{conv}/workspace",
                    h.base
                ))
                .bearer_auth(user["token"].as_str().unwrap())
                .json(&json!({"kind":kind}))
                .send()
                .await?;
            let status = response.status();
            let value: Value = response.json().await?;
            // Automatic post-turn checkpointing can win the admission race.
            if status == 409 && value["error"] == "WORKSPACE_BUSY" {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            ensure!(status.is_success(), "workspace API {status}: {value}");
            accepted = Some(value);
            break;
        }
        let v = accepted.context("workspace stayed busy")?;
        let id = Uuid::parse_str(v["id"].as_str().context("JOB_ID")?)?;
        for _ in 0..600 {
            let (state, result): (String, Option<Value>) =
                sqlx::query_as("SELECT state,result FROM workspace_jobs WHERE id=$1")
                    .bind(id)
                    .fetch_one(&h.db)
                    .await?;
            if state == "completed" {
                return Ok(());
            }
            ensure!(state != "failed", "job failed: {result:?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("job timeout")
    }
    job(&h, &user, org, conv, "checkpoint").await?;
    ryzhub_execution::call(
        host,
        &exec("printf lost > /workspace/untracked", "post-watermark"),
    )
    .await?;
    // Corrupt the primary object; restore must verify and repair it from secondary storage.
    let manifest:Value=sqlx::query_scalar("SELECT c.manifest FROM checkpoints c JOIN workspace_heads w ON w.checkpoint_id=c.id WHERE w.conversation_id=$1").bind(conv).fetch_one(&h.db).await?;
    let key = manifest["sha256"].as_str().unwrap();
    let blob_path = std::path::Path::new(&h.app.config.work.storage_root)
        .join(org.to_string())
        .join(key);
    let archive = std::fs::read(&blob_path)?;
    let decoded = zstd::decode_all(archive.as_slice())?;
    assert!(
        !decoded
            .windows(b"must-not-reach-openai".len())
            .any(|w| w == b"must-not-reach-openai")
    );
    std::fs::write(&blob_path, b"corrupted object")?;
    job(&h, &user, org, conv, "restore").await?;
    let tasks = Process::new("docker")
        .args([
            "exec",
            "ryzhub-m1-execution",
            "ctr",
            "--address",
            "/run/ryzhub-containerd.sock",
            "--namespace",
            "ryzhub",
            "tasks",
            "list",
        ])
        .output()
        .await?;
    ensure!(
        tasks.status.success(),
        "cannot inspect stopped task reclamation"
    );
    ensure!(
        !String::from_utf8_lossy(&tasks.stdout)
            .lines()
            .any(|line| line.split_whitespace().next() == Some(sandbox.to_string().as_str())),
        "stopped source task must release its shim"
    );
    assert_eq!(std::fs::read(&blob_path)?, archive);
    reqwest::Client::new().post(format!("{}/orgs/{org}/credentials",h.base)).bearer_auth(user["token"].as_str().unwrap()).json(&json!({"name":"OpenAI BYOK fixture","provider":"openai","api_key":"openai-fixture-key"})).send().await?.error_for_status()?;
    let restored: Uuid = sqlx::query_scalar(
        "SELECT id FROM sandboxes WHERE conversation_id=$1 AND persistent AND state='running'",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    let restored_epoch: i64 = sqlx::query_scalar("SELECT exec_epoch FROM sandboxes WHERE id=$1")
        .bind(restored)
        .fetch_one(&h.db)
        .await?;
    let result=guest_exec(host,restored,restored_epoch,"verify-restored","cat /workspace/untracked; stat -c %a /workspace/untracked; test ! -d /etc/periodic; test -d /etc/renamed-periodic; stat -c %a /etc/renamed-periodic").await?;
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["stdout_head"], "checkpoint751\n700\n");
    h.send(&user, conv, message("hello again")).await?;
    for _ in 0..200 {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM runs WHERE conversation_id=$1")
            .bind(conv)
            .fetch_one(&h.db)
            .await?;
        if count == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let second = h.wait_state(conv, "completed").await?;
    assert_ne!(second, run);
    assert!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .any(|(body, _)| body.to_string().contains("平台恢复说明"))
    );
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(v, _)| v.get("model").is_some())
            .unwrap()
            .1["authorization"],
        "Bearer openai-fixture-key"
    );
    let threads: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT native_thread_id) FROM sandboxes WHERE conversation_id=$1",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(threads, 1);
    wait_workspace_idle(&h, conv).await?;
    job(&h, &user, org, conv, "restore").await?;
    let live = ryzhub_execution::call(host, &HostCommand::Status).await?;
    assert!(
        !live["running"]
            .as_array()
            .unwrap()
            .contains(&json!(restored)),
        "repeated restore must stop the current writer"
    );
    owner.kill().await?;
    h.close().await
}
async fn wait_workspace_idle(h: &Harness, conv: Uuid) -> anyhow::Result<()> {
    for _ in 0..600 {
        let ready:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_heads WHERE conversation_id=$1) AND NOT EXISTS(SELECT 1 FROM workspace_jobs WHERE conversation_id=$1 AND state IN ('queued','running'))").bind(conv).fetch_one(&h.db).await?;
        if ready {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("automatic turn checkpoint did not complete")
}
async fn mcp_upstream(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    requests.lock().unwrap().push((body.clone(), headers));
    Json(if body["method"] == "initialize" {
        json!({"jsonrpc":"2.0","id":body["id"],"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}})
    } else {
        json!({"jsonrpc":"2.0","id":body["id"],"result":{"content":[{"type":"text","text":"tool executed"}],"isError":false}})
    })
}
async fn guest_exec(
    host: &std::path::Path,
    sandbox: Uuid,
    epoch: i64,
    op: &str,
    cmd: &str,
) -> anyhow::Result<Value> {
    use ryzhub_execution::{ExecSpec, GuestCommand, GuestRequest, HostCommand};
    let call = |command| HostCommand::Guest {
        request: GuestRequest {
            sandbox_id: sandbox,
            exec_epoch: epoch as u64,
            command,
        },
    };
    let mut result = ryzhub_execution::call(
        host,
        &call(GuestCommand::Exec {
            op_id: op.into(),
            spec: ExecSpec {
                cmd: cmd.into(),
                cwd: "/workspace".into(),
                env: Default::default(),
                timeout_ms: 10000,
                background: false,
                pty: false,
            },
        }),
    )
    .await?;
    for _ in 0..200 {
        if result["state"] != "running" {
            return Ok(result);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        result = ryzhub_execution::call(
            host,
            &call(GuestCommand::Poll {
                op_id: op.into(),
                wait_ms: 0,
            }),
        )
        .await?;
    }
    anyhow::bail!("EXEC_TIMEOUT")
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS/containerd and actual Codex"]
async fn bypassing_harness_still_requires_one_use_mcp_approval() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    h.send(&user, conv, message("manual gateway test")).await?;
    let lease = h
        .app
        .store
        .claim(org, conv, Uuid::now_v7(), 60_000)
        .await?
        .unwrap();
    owner::process_with_config(&h.app.store, &lease, &h.app.config).await?;
    let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    let sandbox = ryzhub_server::work::ensure_work(&h.app, &lease, run).await?;
    // The same kind of key the guest holds, issued directly: the caller bypasses the harness.
    let epoch: i64 = sqlx::query_scalar("SELECT lease_epoch FROM sandboxes WHERE id=$1")
        .bind(sandbox)
        .fetch_one(&h.db)
        .await?;
    let token = keys::issue_sandbox_key(
        &h.app.store,
        keys::SandboxKeyRequest {
            org_id: org,
            user_id: id(&user, "user_id"),
            conversation_id: conv,
            run_id: run,
            sandbox_id: sandbox,
            lease_epoch: epoch,
            models: vec![h.app.config.work.model.clone()],
            credential_id: None,
        },
    )
    .await?
    .token;
    let request = json!({"jsonrpc":"2.0","id":"once","method":"tools/call","params":{"name":"write_note","arguments":{"text":"approved parameters"}}});
    let url = format!("{}/mcp", h.gateway);
    let client = reqwest::Client::new();
    let pending = tokio::spawn(client.post(&url).bearer_auth(&token).json(&request).send());
    for _ in 0..100 {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mcp_calls WHERE run_id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
        if count == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .all(|(v, _)| v["method"] != "tools/call")
    );
    ryzhub_server::mcp::project(&h.app, &lease, run).await?;
    let (action, epoch) = h.action(run).await?;
    h.send(
        &user,
        conv,
        Command::DecideApproval(DecideApproval {
            action_id: action.to_string(),
            run_epoch: epoch as u64,
            decision: "allow".into(),
        }),
    )
    .await?;
    owner::process_with_config(&h.app.store, &lease, &h.app.config).await?;
    let result: Value = pending.await??.json().await?;
    assert!(result.get("error").is_none(), "{result}");
    let again: Value = client
        .post(&url)
        .bearer_auth(&token)
        .json(&request)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(result, again);
    let mut changed = request;
    changed["params"]["arguments"]["text"] = json!("different");
    let denied: Value = client
        .post(&url)
        .bearer_auth(&token)
        .json(&changed)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(denied["error"]["message"], "REQUEST_ID_REUSED");
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(v, _)| v["method"] == "tools/call")
            .count(),
        1
    );
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS/containerd and actual Codex"]
async fn owner_restarts_do_not_restart_or_interrupt_native_model_stream() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut first = h.owner()?;
    h.send(&user, conv, message("slow native response")).await?;
    for _ in 0..200 {
        if h.requests
            .lock()
            .unwrap()
            .iter()
            .any(|(v, _)| v.get("model").is_some())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    first.kill().await?;
    let mut second = h.owner()?;
    let run = h.wait_state(conv, "completed").await?;
    let epoch: i64 = sqlx::query_scalar("SELECT exec_epoch FROM runs WHERE id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    assert!(epoch >= 2);
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(v, _)| v.get("model").is_some())
            .count(),
        1
    );
    let usage: (String, bool) =
        sqlx::query_as("SELECT status,estimated FROM usage_records WHERE run_id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
    assert_eq!(usage, ("completed".into(), false));
    second.kill().await?;
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS"]
async fn restore_pin_blocks_expired_checkpoint_gc_and_secondary_repairs_primary()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    h.send(&user, conv, message("checkpoint metadata fixture"))
        .await?;
    let lease = h
        .app
        .store
        .claim(org, conv, Uuid::now_v7(), 60_000)
        .await?
        .unwrap();
    owner::process_with_config(&h.app.store, &lease, &h.app.config).await?;
    let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    let sandbox = Uuid::now_v7();
    sqlx::query("INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent) VALUES($1,$2,$3,$4,1,1,'stopped','{}',true)").bind(sandbox).bind(org).bind(conv).bind(run).execute(&h.db).await?;
    use sha2::{Digest, Sha256};
    let bytes = b"immutable archive fixture";
    let key = format!("{:x}", Sha256::digest(bytes));
    let p = ryzhub_server::workspace::blob::BlobStore::configured(&h.app.config.work.storage_root)?
        .scoped(org);
    let s = ryzhub_server::workspace::blob::BlobStore::configured(
        h.app.config.work.secondary_root.as_deref().unwrap(),
    )?
    .scoped(org);
    p.put(&key, bytes).await?;
    s.put(&key, bytes).await?;
    let checkpoint = Uuid::now_v7();
    sqlx::query("INSERT INTO checkpoints(id,org_id,conversation_id,sandbox_id,manifest,channel_seq,gseq,state,delete_after) VALUES($1,$2,$3,$4,$5,0,0,'tombstoned',now()-interval '1 second')").bind(checkpoint).bind(org).bind(conv).bind(sandbox).bind(json!({"sha256":key})).execute(&h.db).await?;
    let pin = Uuid::now_v7();
    let mut restoring = h.app.store.tx(org).await?;
    sqlx::query("SELECT id FROM checkpoints WHERE org_id=$1 AND id=$2 FOR UPDATE")
        .bind(org)
        .bind(checkpoint)
        .fetch_one(&mut *restoring)
        .await?;
    let gc_app = h.app.clone();
    let mut collecting =
        tokio::spawn(async move { ryzhub_server::workspace::gc(&gc_app, org, checkpoint).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut collecting)
            .await
            .is_err()
    );
    sqlx::query("INSERT INTO restore_pins(id,org_id,checkpoint_id) VALUES($1,$2,$3)")
        .bind(pin)
        .bind(org)
        .bind(checkpoint)
        .execute(&mut *restoring)
        .await?;
    restoring.commit().await?;
    assert!(
        !collecting.await??,
        "GC must recheck a pin committed by its concurrent restore"
    );
    assert_eq!(p.get(&key).await?, bytes);
    p.delete(&key).await?;
    p.put(&key, &s.get(&key).await?).await?;
    assert_eq!(p.get(&key).await?, bytes);
    sqlx::query("DELETE FROM restore_pins WHERE id=$1")
        .bind(pin)
        .execute(&h.db)
        .await?;
    let head = Uuid::now_v7();
    let superseded = Uuid::now_v7();
    for id in [head, superseded] {
        sqlx::query("INSERT INTO checkpoints(id,org_id,conversation_id,sandbox_id,manifest,channel_seq,gseq,created_at) VALUES($1,$2,$3,$4,$5,0,0,now()-interval '40 days')")
            .bind(id).bind(org).bind(conv).bind(sandbox).bind(json!({"sha256":key})).execute(&h.db).await?;
    }
    sqlx::query(
        "INSERT INTO workspace_heads(org_id,conversation_id,checkpoint_id) VALUES($1,$2,$3)",
    )
    .bind(org)
    .bind(conv)
    .bind(head)
    .execute(&h.db)
    .await?;
    let mut app = h.app.clone();
    Arc::make_mut(&mut app.config)
        .work
        .checkpoint_retention_days = 30;
    Arc::make_mut(&mut app.config).work.checkpoint_gc_grace_secs = 60;
    ryzhub_server::workspace::collect_org(&app, org).await?;
    let states: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id,state FROM checkpoints WHERE org_id=$1")
            .bind(org)
            .fetch_all(&h.db)
            .await?;
    assert!(states.contains(&(checkpoint, "deleted".into())));
    assert!(states.contains(&(superseded, "tombstoned".into())));
    assert!(states.contains(&(head, "committed".into())));
    assert_eq!(
        p.get(&key).await?,
        bytes,
        "the shared blob and current head survive GC"
    );
    assert!(
        !ryzhub_server::workspace::gc(&app, org, superseded).await?,
        "grace period is recoverable"
    );
    sqlx::query("UPDATE checkpoints SET delete_after=now()-interval '1 second' WHERE id=$1")
        .bind(superseded)
        .execute(&h.db)
        .await?;
    let mut worker = h.owner()?;
    for _ in 0..100 {
        let state: String = sqlx::query_scalar("SELECT state FROM checkpoints WHERE id=$1")
            .bind(superseded)
            .fetch_one(&h.db)
            .await?;
        if state == "deleted" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    worker.kill().await?;
    let state: String = sqlx::query_scalar("SELECT state FROM checkpoints WHERE id=$1")
        .bind(superseded)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(
        state, "deleted",
        "the production collector must run in the Owner role"
    );
    let unrelated =
        ryzhub_server::workspace::blob::BlobStore::configured(&app.config.work.storage_root)?
            .scoped(Uuid::now_v7());
    unrelated.put(&key, bytes).await?;
    sqlx::query("DELETE FROM workspace_heads WHERE org_id=$1")
        .bind(org)
        .execute(&h.db)
        .await?;
    Arc::make_mut(&mut app.config).work.checkpoint_gc_grace_secs = 0;
    ryzhub_server::workspace::collect_org(&app, org).await?;
    assert!(p.get(&key).await.is_err());
    assert!(s.get(&key).await.is_err());
    assert_eq!(
        unrelated.get(&key).await?,
        bytes,
        "another tenant's identical blob survives"
    );
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS/containerd and actual Codex"]
async fn node_lease_expiry_restores_committed_rpo_and_invalidates_later_operations()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    let mut owner = h.owner()?;
    h.send(&user, conv, message("durable prefix")).await?;
    let first = h.wait_state(conv, "completed").await?;
    let job: Value = reqwest::Client::new()
        .post(format!(
            "{}/orgs/{org}/conversations/{conv}/workspace",
            h.base
        ))
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"kind":"checkpoint"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let checkpoint = id(&job, "id");
    for _ in 0..300 {
        let state: String = sqlx::query_scalar("SELECT state FROM workspace_jobs WHERE id=$1")
            .bind(checkpoint)
            .fetch_one(&h.db)
            .await?;
        if state == "completed" {
            break;
        }
        ensure!(state != "failed", "CHECKPOINT_FAILED");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (old, epoch): (Uuid, i64) = sqlx::query_as(
        "SELECT id,exec_epoch FROM sandboxes WHERE conversation_id=$1 AND state='running'",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    h.send(&user, conv, message("slow uncommitted operation"))
        .await?;
    for _ in 0..200 {
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=$1 AND id<>$2 AND native_turn_id IS NOT NULL)").bind(conv).bind(first).fetch_one(&h.db).await?;
        if active {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ryzhub_execution::call(
        std::path::Path::new(&h.app.config.sandbox.host_socket),
        &ryzhub_execution::HostCommand::Stop {
            sandbox_id: old,
            exec_epoch: epoch as u64,
        },
    )
    .await?;
    let mut recovered = false;
    for _ in 0..500 {
        let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sandboxes WHERE conversation_id=$1 AND id<>$2 AND state='running')").bind(conv).bind(old).fetch_one(&h.db).await?;
        if live {
            recovered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ensure!(recovered, "AUTOMATIC_RECOVERY_TIMEOUT");
    let invalid: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items WHERE conversation_id=$1 AND state='invalidated'",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    assert!(invalid > 0);
    let notice:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM items WHERE conversation_id=$1 AND kind='notice' AND text LIKE '%水位后的操作%')").bind(conv).fetch_one(&h.db).await?;
    assert!(notice);
    let revoked: bool = sqlx::query_scalar(
        "SELECT bool_and(revoked_at IS NOT NULL) FROM virtual_keys WHERE sandbox_id=$1",
    )
    .bind(old)
    .fetch_one(&h.db)
    .await?;
    assert!(revoked);
    owner.kill().await?;
    h.close().await
}
#[tokio::test]
#[ignore = "requires local S3 wire fixture verified by AWS botocore"]
async fn s3_sigv4_storage_roundtrip_and_authentication() -> anyhow::Result<()> {
    let s3_url =
        std::env::var("RYZHUB_TEST_S3_URL").unwrap_or_else(|_| "http://127.0.0.1:59000".into());
    let mut config = tempfile::NamedTempFile::new()?;
    write!(
        config,
        "endpoint = \"{s3_url}\"\nbucket = \"ryzhub-test\"\nregion = \"us-east-1\"\naccess_env = \"RYZHUB_TEST_S3_ACCESS\"\nsecret_env = \"RYZHUB_TEST_S3_SECRET\"\n"
    )?;
    let store = ryzhub_server::workspace::blob::BlobStore::configured(&format!(
        "s3config:{}",
        config.path().display()
    ))?
    .scoped(Uuid::now_v7());
    use sha2::{Digest, Sha256};
    let body = format!("archive {}", Uuid::now_v7());
    let key = format!("{:x}", Sha256::digest(body.as_bytes()));
    store.put(&key, body.as_bytes()).await?;
    assert_eq!(store.get(&key).await?, body.as_bytes());
    store.delete(&key).await?;
    assert!(store.get(&key).await.is_err());
    // Objects larger than one part go up as a multipart upload.
    let large: Vec<u8> = (0..16 * 1024 * 1024 + 1).map(|i| (i % 251) as u8).collect();
    let large_key = format!("{:x}", Sha256::digest(&large));
    store.put(&large_key, &large).await?;
    assert_eq!(store.get(&large_key).await?, large);
    store.delete(&large_key).await?;
    config.as_file_mut().set_len(0)?;
    use std::io::{Seek, SeekFrom};
    config.as_file_mut().seek(SeekFrom::Start(0))?;
    write!(
        config,
        "endpoint = \"{s3_url}\"\nbucket = \"ryzhub-test\"\nregion = \"us-east-1\"\naccess_env = \"RYZHUB_TEST_S3_ACCESS\"\nsecret_env = \"RYZHUB_TEST_UPSTREAM_KEY\"\n"
    )?;
    let bad = ryzhub_server::workspace::blob::BlobStore::configured(&format!(
        "s3config:{}",
        config.path().display()
    ))?;
    assert!(bad.put(&key, body.as_bytes()).await.is_err());
    Ok(())
}
#[tokio::test]
#[ignore = "requires actual Codex, PostgreSQL/NATS/containerd"]
async fn native_command_approval_is_durable_and_executes_once() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut owner = h.owner()?;
    h.send(&user, conv, message("native-exec")).await?;
    let run = h.wait_state(conv, "requires_action").await?;
    let (action, epoch) = h.action(run).await?;
    let (sandbox, exec_epoch): (Uuid, i64) =
        sqlx::query_as("SELECT id,exec_epoch FROM sandboxes WHERE run_id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
    let before = guest_exec(
        std::path::Path::new(&h.app.config.sandbox.host_socket),
        sandbox,
        exec_epoch,
        "before-approval",
        "test ! -f /workspace/approval-marker",
    )
    .await?;
    assert_eq!(before["exit_code"], 0);
    h.send(
        &user,
        conv,
        Command::DecideApproval(DecideApproval {
            action_id: action.to_string(),
            run_epoch: epoch as u64,
            decision: "allow".into(),
        }),
    )
    .await?;
    h.wait_state(conv, "completed").await?;
    let result = guest_exec(
        std::path::Path::new(&h.app.config.sandbox.host_socket),
        sandbox,
        exec_epoch,
        "after-approval",
        "cat /workspace/approval-marker",
    )
    .await?;
    assert_eq!(result["stdout_head"], "native-once");
    let tool:String=sqlx::query_scalar("SELECT text FROM items WHERE run_id=$1 AND kind='tool' AND state='completed' ORDER BY last_seq DESC LIMIT 1").bind(run).fetch_one(&h.db).await?;
    assert!(tool.contains("native-once"));
    owner.kill().await?;
    h.close().await
}
