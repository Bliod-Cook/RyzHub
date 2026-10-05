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
use ryzhub_server::{app::App, config::Config, http::router, llm::gateway, store::Store};
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
    let prompt = last.as_str().map(str::to_owned).unwrap_or_else(|| {
        last.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    });
    let claude = body["tools"]
        .as_array()
        .is_some_and(|a| a.iter().any(|t| t["name"] == "Bash"));
    let result = last
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"));
    let mut content = vec![
        json!({"type":"thinking","thinking":"opaque reasoning","signature":"signed-block"}),
        json!({"type":"redacted_thinking","data":"opaque-data"}),
    ];
    if !result
        && body.get("tools").is_some()
        && (if claude {
            prompt.contains("M3_EXEC")
        } else {
            prompt.contains("exec")
        })
    {
        let command = if prompt.contains("proxyexec") {
            r#"sleep 2; wget -qO- --header 'Content-Type: application/json' --post-data '{"model":body["model"],"max_tokens":32,"stream":true,"messages":[{"role":"user","content":"guest local proxy"}]}' http://127.0.0.1:8090/v1/messages"#
        } else {
            "printf once >> marker; sleep 1; printf '真实沙箱结果'"
        };
        content.push(json!({"type":"text","text":"准备执行工具"}));
        content.push(json!({"type":"tool_use","id":"call_exec","name":if claude {"Bash"} else {"exec"},"input":if claude{json!({"command":command,"description":"Local execution"})}else{json!({"cmd":command,"timeout_ms":10000})}}));
    } else if !result
        && body.get("tools").is_some()
        && (if claude {
            prompt.contains("M3_QUESTION")
        } else {
            prompt.contains("question")
        })
    {
        content.push(json!({"type":"tool_use","id":"call_question","name":if claude {"AskUserQuestion"}else{"ask_user"},"input":if claude{json!({"questions":[{"question":"选择一种颜色","header":"颜色","options":[{"label":"Blue","description":"Blue"},{"label":"Green","description":"Green"}],"multiSelect":false}]})}else{json!({"question":"选择一种颜色"})}}));
    } else {
        content.push(json!({"type":"text","text":if result{"工具结果已用于回答"}else if body.get("tools").is_none(){"只读旁问完成"}else{"本地协议联调：你好 Rust"}}));
    }
    if body["stream"] != true {
        return axum::response::Response::builder().header("content-type","application/json").body(Body::from(json!({"id":format!("msg_{}",Uuid::now_v7()),"type":"message","role":"assistant","model":body["model"],"content":content,"stop_reason":"end_turn","usage":{"input_tokens":25,"output_tokens":8}}).to_string())).unwrap();
    }
    let tool = content.iter().any(|b| b["type"] == "tool_use");
    let mut events = vec![
        json!({"type":"message_start","message":{"id":format!("msg_{}",Uuid::now_v7()),"type":"message","role":"assistant","content":[],"model":body["model"],"usage":{"input_tokens":25,"output_tokens":1,"cache_read_input_tokens":12}}}),
        json!({"type":"ping"}),
    ];
    for (index, block) in content.iter().enumerate() {
        let mut start = block.clone();
        if block["type"] == "text" {
            start["text"] = json!("");
        }
        if block["type"] == "tool_use" {
            start["input"] = json!({});
        }
        if block["type"] == "thinking" {
            start["thinking"] = json!("");
            start["signature"] = json!("");
        }
        events.push(json!({"type":"content_block_start","index":index,"content_block":start}));
        if block["type"] == "text" {
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":block["text"]}}));
        }
        if block["type"] == "thinking" {
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":block["thinking"]}}));
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":block["signature"]}}));
        }
        if block["type"] == "tool_use" {
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":block["input"].to_string()}}));
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
        config.work.model = "gpt-5.4".into();
        config.work.claude_model = "claude-sonnet-4-6".into();
        config.llm.responses_model = "gpt-5.4".into();
        config.llm.models = vec![ryzhub_server::config::ModelRoute {
            id: "claude-opus-4-6".into(),
            protocol: "anthropic".into(),
            harnesses: vec!["claude".into(), "chat".into()],
        }];
        config.llm.models.push(ryzhub_server::config::ModelRoute {
            id: "gpt-5.2".into(),
            protocol: "responses".into(),
            harnesses: vec!["codex".into()],
        });
        config.policy.allow_full_auto = Some(false);
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
        config.sandbox.image = std::env::var("TEST_M3_IMAGE").unwrap_or_else(|_| {
            std::fs::read_to_string(workspace.join("var/m3/image.txt"))
                .unwrap_or_default()
                .trim()
                .into()
        });
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
                        .route("/v1/responses/compact", post(compact))
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
                    let events:Vec<Value>=sqlx::query_scalar("SELECT payload FROM guest_events WHERE sandbox_id=(SELECT sandbox_id FROM runs WHERE id=$1) AND payload::text LIKE '%error%' ORDER BY gseq DESC LIMIT 4").bind(run).fetch_all(&self.db).await?;
                    anyhow::bail!("run failed: {error:?}; native errors: {events:?}");
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
    let text = "Codex / Responses 本地协议联调成功：你好 Rust。";
    let mut item = json!({"id":mid,"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]});
    let inputs = body["input"].as_array().unwrap();
    let user_at = inputs
        .iter()
        .rposition(|i| i["role"] == "user")
        .unwrap_or(0);
    let prompt = inputs.get(user_at).unwrap_or(&Value::Null).to_string();
    let tool_result = inputs
        .iter()
        .skip(user_at)
        .any(|i| i["type"] == "function_call_output");
    if prompt.contains("M3_FORK_BOUNDARY") {
        return regressions::tool_turn_response(&body, tool_result);
    }
    let exec = prompt.contains("native-exec") && !tool_result;
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
        } else if names.contains(&"exec") {
            (
                "exec",
                json!({"cmd":"printf responses-once >> /workspace/approval-marker; cat /workspace/approval-marker"}),
            )
        } else {
            (
                "shell_command",
                json!({"command":"printf native-once >> /workspace/approval-marker; cat /workspace/approval-marker","workdir":"/workspace"}),
            )
        };
        item = json!({"id":mid,"type":"function_call","call_id":"fixture-native-command","name":name,"arguments":args.to_string(),"status":"completed"});
    }
    let reasoning = json!({"id":format!("rs_{}",Uuid::now_v7()),"type":"reasoning","summary":[],"encrypted_content":"opaque-reasoning"});
    let response = json!({"id":rid,"object":"response","created_at":0,"model":body["model"],"status":"completed","output":[item,reasoning],"usage":{"input_tokens":31,"output_tokens":9,"total_tokens":40,"input_tokens_details":{"cached_tokens":7}}});
    let delay = if prompt.contains("slow") { 300 } else { 50 };
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
    events.push(json!({"type":"response.output_item.done","output_index":1,"item":reasoning}));
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

async fn compact(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    requests.lock().unwrap().push((body.clone(), headers));
    Json(
        json!({"id":"cmp_local","object":"response.compaction","output":[{"type":"compaction","id":"cp_1","encrypted_content":"opaque-compaction"}],"usage":{"input_tokens":20,"output_tokens":3}}),
    )
}
impl Harness {
    async fn configured(
        &self,
        user: &Value,
        mode: &str,
        settings: Value,
        project: Option<Uuid>,
    ) -> anyhow::Result<Uuid> {
        let ctx = self
            .app
            .store
            .context(id(user, "user_id"), id(user, "org_id"))
            .await?;
        Ok(self
            .app
            .store
            .create_configured(&ctx, "M3", mode, settings, project, Some(&self.app.config))
            .await?
            .id)
    }
    async fn submit(&self, user: &Value, conv: Uuid, command: Command) -> anyhow::Result<Uuid> {
        use prost::Message;
        println!("command {:?}", command);
        let cid = self.send(user, conv, command).await?;
        for _ in 0..600 {
            let result: Option<Vec<u8>> =
                sqlx::query_scalar("SELECT result FROM commands WHERE cid=$1")
                    .bind(cid)
                    .fetch_one(&self.db)
                    .await?;
            if let Some(result) = result {
                let frame = Frame::decode(result.as_slice())?;
                ensure!(
                    matches!(frame.body, Some(frame::Body::CommandAck(_))),
                    "command rejected: {:?}",
                    frame.body
                );
                return Ok(cid);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("COMMAND_TIMEOUT")
    }
    async fn json(
        &self,
        user: &Value,
        path: &str,
        method: reqwest::Method,
        body: Value,
    ) -> anyhow::Result<Value> {
        for _ in 0..300 {
            let r = reqwest::Client::new()
                .request(method.clone(), format!("{}{}", self.base, path))
                .bearer_auth(user["token"].as_str().unwrap())
                .json(&body)
                .send()
                .await?;
            let status = r.status();
            let value: Value = r.json().await?;
            if status == 409 && path.ends_with("/workspace") && value["error"] == "WORKSPACE_BUSY" {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            ensure!(status.is_success(), "API {status}: {value}");
            return Ok(value);
        }
        anyhow::bail!("WORKSPACE_TIMEOUT")
    }

    async fn idle(&self, conv: Uuid) -> anyhow::Result<()> {
        for _ in 0..1000 {
            let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE conversation_id=$1 AND state IN ('queued','running'))").bind(conv).fetch_one(&self.db).await?;
            if !busy {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("WORKSPACE_TIMEOUT")
    }
    async fn fork_result(&self, cid: Uuid) -> anyhow::Result<Uuid> {
        for _ in 0..1000 {
            let (state, result): (String, Option<Value>) =
                sqlx::query_as("SELECT state,result FROM conversation_operations WHERE id=$1")
                    .bind(cid)
                    .fetch_one(&self.db)
                    .await?;
            if state == "completed" {
                return Ok(id(&result.unwrap(), "conversation_id"));
            }
            ensure!(state != "failed", "FORK_FAILED: {result:?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("FORK_TIMEOUT")
    }
}
#[tokio::test]
#[ignore = "requires local PostgreSQL, NATS and containerd"]
async fn responses_native_history_projects_and_commands() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let mut owner = h.owner()?;
    let outcome=async {
 let project=h.json(&user,&format!("/orgs/{org}/projects"),reqwest::Method::POST,json!({"name":"Private knowledge","instructions":"PROJECT_SENTINEL","memory_mode":"project_only","defaults":{"mode":"chat","protocol":"responses","model":"gpt-5.4"}})).await?;let project=id(&project,"id");
 h.json(&user,&format!("/orgs/{org}/projects/{project}/files"),reqwest::Method::POST,json!({"name":"knowledge.txt","content":"Project answer: 42"})).await?;
 let other=h.user().await?;let denied=reqwest::Client::new().get(format!("{}/orgs/{org}/projects/{project}/files",h.base)).bearer_auth(other["token"].as_str().unwrap()).send().await?;ensure!(!denied.status().is_success());
 let immutable=reqwest::Client::new().patch(format!("{}/orgs/{org}/projects/{project}",h.base)).bearer_auth(user["token"].as_str().unwrap()).json(&json!({"name":"bad","memory_mode":"default"})).send().await?;ensure!(immutable.status()==400);
 let defaults=h.json(&user,&format!("/orgs/{org}/projects"),reqwest::Method::POST,json!({"name":"Defaults","defaults":{"mode":"work","harness":"claude","model":"claude-sonnet-4-6"}})).await?;
 let inherited=h.json(&user,&format!("/orgs/{org}/conversations"),reqwest::Method::POST,json!({"title":"Inherit defaults","project_id":defaults["id"]})).await?;ensure!(inherited["mode"]=="work" && inherited["run_config"]["harness"]=="claude");
 let conv=h.configured(&user,"chat",json!({}),Some(project)).await?;
 ensure!(h.configured(&user,"chat",json!({"approval":"full_auto"}),None).await.unwrap_err().to_string().contains("POLICY_DENIED"));
 h.submit(&user,conv,message("native-exec")).await?;let run=h.wait_state(conv,"requires_action").await?;let (action,epoch)=h.action(run).await?;
 h.submit(&user,conv,Command::DecideApproval(DecideApproval{action_id:action.to_string(),run_epoch:epoch as u64,decision:"allow".into()})).await?;h.wait_state(conv,"completed").await?;
 let steps:Vec<Value>=sqlx::query_scalar("SELECT native_response FROM loop_steps WHERE run_id=$1 ORDER BY step_no").bind(run).fetch_all(&h.db).await?;ensure!(steps.len()==2 && steps[0]["output"][1]["encrypted_content"]=="opaque-reasoning");
 {let requests=h.requests.lock().unwrap();let next=&requests.iter().rfind(|(b,_)|b.get("input").is_some()).unwrap().0;ensure!(next["input"].to_string().contains("opaque-reasoning") && next["input"].to_string().contains("function_call_output"));ensure!(next["instructions"].as_str().unwrap().contains("PROJECT_SENTINEL"));}
 let mut tx=h.app.store.tx(org).await?;let found=ryzhub_server::projects::search(&mut tx,org,run,"42").await?;ensure!(found["matches"][0]["file"]=="knowledge.txt");tx.commit().await?;
 h.json(&user,&format!("/orgs/{org}/projects/{project}/files"),reqwest::Method::POST,json!({"name":"knowledge.txt","content":"Project answer: 84"})).await?;
 let mut tx=h.app.store.tx(org).await?;let old=ryzhub_server::projects::search(&mut tx,org,run,"42").await?;ensure!(!old["matches"].as_array().unwrap().is_empty(),"run must retain its project snapshot");tx.commit().await?;
 let (sandbox,epoch):(Uuid,i64)=sqlx::query_as("SELECT id,exec_epoch FROM sandboxes WHERE run_id=$1").bind(run).fetch_one(&h.db).await?;
 // Ephemeral Chat sandbox may already be stopped at this boundary; Work mount is tested below.
 let _=(sandbox,epoch);
 h.submit(&user,conv,Command::Compact(Compact{})).await?;let compact_run=h.wait_state(conv,"completed").await?;
 let response:Value=sqlx::query_scalar("SELECT native_response FROM loop_steps WHERE run_id=$1").bind(compact_run).fetch_one(&h.db).await?;ensure!(response["output"][0]["type"]=="compaction");
 h.submit(&user,conv,message("after compact")).await?;let run=h.wait_state(conv,"completed").await?;
 let mut tx=h.app.store.tx(org).await?;let updated=ryzhub_server::projects::search(&mut tx,org,run,"84").await?;ensure!(!updated["matches"].as_array().unwrap().is_empty(),"next run must use edited files");tx.commit().await?;
 let request:Value=sqlx::query_scalar("SELECT request_messages FROM loop_steps WHERE run_id=$1 AND step_no=0").bind(run).fetch_one(&h.db).await?;ensure!(request.to_string().contains("opaque-compaction"));
 let item:Uuid=sqlx::query_scalar("SELECT assistant_item_id FROM runs WHERE id=$1").bind(run).fetch_one(&h.db).await?;
 let fork=h.submit(&user,conv,Command::Fork(Fork{item_id:item.to_string()})).await?;let fork=h.fork_result(fork).await?;
 h.submit(&user,fork,message("forked native continuation")).await?;h.wait_state(fork,"completed").await?;
 h.json(&user,&format!("/orgs/{org}/conversations/{conv}/project"),reqwest::Method::POST,json!({"project_id":null})).await?;
 h.json(&user,&format!("/orgs/{org}/projects/{project}"),reqwest::Method::DELETE,json!({})).await?;
 let deleted:String=sqlx::query_scalar("SELECT state FROM conversations WHERE id=$1").bind(fork).fetch_one(&h.db).await?;ensure!(deleted=="deletion_pending");
 h.json(&user,&format!("/orgs/{org}/projects/{project}/restore"),reqwest::Method::POST,json!({})).await?;
 h.json(&user,&format!("/orgs/{org}/projects/{project}"),reqwest::Method::DELETE,json!({})).await?;
 ensure!(!ryzhub_server::projects::collect(&h.app,org,project).await?,"recoverable project cannot be purged");
 sqlx::query("UPDATE projects SET deleted_at=now()-interval '31 days' WHERE id=$1").bind(project).execute(&h.db).await?;
 ensure!(ryzhub_server::projects::collect(&h.app,org,project).await?);
 ensure!(!ryzhub_server::projects::collect(&h.app,org,project).await?,"collection is idempotent");
 let remains:i64=sqlx::query_scalar("SELECT count(*) FROM project_files WHERE project_id=$1").bind(project).fetch_one(&h.db).await?;ensure!(remains==0);
 let erased:bool=sqlx::query_scalar("SELECT state='deleted' AND context_seed IS NULL FROM conversations WHERE id=$1").bind(fork).fetch_one(&h.db).await?;ensure!(erased);
 let visible:String=sqlx::query_scalar("SELECT state FROM conversations WHERE id=$1").bind(conv).fetch_one(&h.db).await?;ensure!(visible=="active","moved-out conversation must survive");

 Ok::<_,anyhow::Error>(())
 }.await;
    owner.kill().await?;
    h.close().await?;
    outcome
}
#[tokio::test]
#[ignore = "requires local PostgreSQL, NATS and containerd"]
async fn models_catalog_and_provider_inference() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let mut owner = h.owner()?;
    let outcome = async {
        let catalog: Value = reqwest::Client::new()
            .get(format!("{}/orgs/{org}/models", h.base))
            .bearer_auth(user["token"].as_str().unwrap())
            .send()
            .await?
            .json()
            .await?;
        let chat = catalog["chat"].as_array().context("chat models")?;
        let find = |id: &str| chat.iter().find(|m| m["id"] == id);
        ensure!(find("gpt-5.4").is_some_and(|m| m["provider"] == "openai"), "{catalog}");
        ensure!(find("claude-opus-4-6").is_some_and(|m| m["provider"] == "anthropic"));
        ensure!(find("fixture-model").is_some_and(|m| m["default"] == true));
        ensure!(find("gpt-5.2").is_none(), "Codex-only routes stay out of Chat");
        let claude = catalog["work"]
            .as_array()
            .context("work groups")?
            .iter()
            .find(|g| g["harness"] == "claude")
            .context("claude group")?;
        ensure!(claude["models"].as_array().unwrap().iter().any(|m| {
            m["id"] == "claude-sonnet-4-6"
                && m["default"] == true
                && m["reasoning_levels"].as_array().unwrap().last() == Some(&json!("max"))
        }));

        // Clients send only the model; the server derives the wire protocol.
        let conversations = format!("/orgs/{org}/conversations");
        let create = |run_config: Value, project: Option<Uuid>| {
            h.json(
                &user,
                &conversations,
                reqwest::Method::POST,
                json!({"title":"Model first","mode":"chat","project_id":project,"run_config":run_config}),
            )
        };
        let openai = create(json!({"model":"gpt-5.4"}), None).await?;
        ensure!(openai["run_config"]["protocol"] == "responses", "{openai}");
        let conv = id(&openai, "id");
        h.submit(&user, conv, message("hello by model")).await?;
        h.wait_state(conv, "completed").await?;
        let cross = h
            .submit(
                &user,
                conv,
                Command::UpdateRunConfig(UpdateRunConfig {
                    config_json: json!({"model":"claude-opus-4-6"}).to_string(),
                }),
            )
            .await;
        ensure!(
            cross
                .unwrap_err()
                .to_string()
                .contains("HARNESS_PROTOCOL_IMMUTABLE"),
            "a started chat keeps its provider"
        );

        let anthropic = create(json!({"model":"claude-opus-4-6"}), None).await?;
        ensure!(anthropic["run_config"]["protocol"] == "anthropic", "{anthropic}");
        let conv = id(&anthropic, "id");
        h.submit(&user, conv, message("hello anthropic")).await?;
        h.wait_state(conv, "completed").await?;
        h.submit(
            &user,
            conv,
            Command::UpdateRunConfig(UpdateRunConfig {
                config_json: json!({"model":"fixture-model"}).to_string(),
            }),
        )
        .await?;
        let stored: Value = sqlx::query_scalar("SELECT run_config FROM conversations WHERE id=$1")
            .bind(conv)
            .fetch_one(&h.db)
            .await?;
        ensure!(stored["model"] == "fixture-model" && stored["protocol"] == "anthropic");

        // A project default written by an older client does not fight the chosen model.
        let project = h
            .json(
                &user,
                &format!("/orgs/{org}/projects"),
                reqwest::Method::POST,
                json!({"name":"Legacy defaults","defaults":{"mode":"chat","protocol":"responses"}}),
            )
            .await?;
        let project = id(&project, "id");
        let inherited = create(json!({}), Some(project)).await?;
        ensure!(inherited["run_config"]["protocol"] == "responses", "{inherited}");
        let chosen = create(json!({"model":"claude-opus-4-6"}), Some(project)).await?;
        ensure!(chosen["run_config"]["protocol"] == "anthropic", "{chosen}");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    owner.kill().await?;
    h.close().await?;
    outcome
}
#[tokio::test]
#[ignore = "requires actual Codex and Claude SDK in the M3 sandbox image"]
async fn two_harnesses_steer_commands_fork_and_restore() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let mut owner = h.owner()?;
    let outcome=async {
 for harness in ["codex","claude"] {
  println!("M3 harness: {harness}");
  let conv=h.configured(&user,"work",json!({"harness":harness}),None).await?;
  h.submit(&user,conv,message("slow hello")).await?;let run=h.wait_state(conv,"running").await?;
  // Steer immediately, including while the native session is still being provisioned.
  h.submit(&user,conv,Command::Steer(Steer{text:"second queued instruction".into()})).await?;
  let model=if harness=="claude" {"claude-opus-4-6"}else{"gpt-5.2"};
  h.submit(&user,conv,Command::UpdateRunConfig(UpdateRunConfig{config_json:json!({"model":model,"reasoning":"high","plan":true}).to_string()})).await?;
  let snapshot:Value=sqlx::query_scalar("SELECT run_config FROM runs WHERE id=$1").bind(run).fetch_one(&h.db).await?;ensure!(snapshot["model"]!=model && snapshot["plan"]==false,"settings must not alter the active run");
  h.wait_state(conv,"completed").await?;
  let steer:String=sqlx::query_scalar("SELECT state FROM items WHERE run_id=$1 AND kind='steer'").bind(run).fetch_one(&h.db).await?;ensure!(steer=="handed","{harness} steer: {steer}");
  if harness=="claude" {let turns:i64=sqlx::query_scalar("SELECT count(*) FROM native_turns WHERE run_id=$1").bind(run).fetch_one(&h.db).await?;ensure!(turns==2,"Claude must queue into a second native turn: {turns}");}
  h.idle(conv).await?;
  h.submit(&user,conv,message("plan a change")).await?;let configured=h.wait_state(conv,"completed").await?;
  let settings:Value=sqlx::query_scalar("SELECT run_config FROM runs WHERE id=$1").bind(configured).fetch_one(&h.db).await?;ensure!(settings["plan"]==true && settings["reasoning"]=="high" && settings["model"]==model);
  ensure!(h.requests.lock().unwrap().iter().any(|(b,_)|b["model"]==model),"native model switch must reach upstream");
  h.idle(conv).await?;
  h.submit(&user,conv,Command::UpdateRunConfig(UpdateRunConfig{config_json:json!({"plan":false}).to_string()})).await?;
  h.submit(&user,conv,message(if harness=="codex"{"native-exec"}else{"M3_EXEC"})).await?;let run=h.wait_state(conv,"requires_action").await?;let (action,epoch)=h.action(run).await?;
  h.submit(&user,conv,Command::DecideApproval(DecideApproval{action_id:action.to_string(),run_epoch:epoch as u64,decision:"allow".into()})).await?;h.wait_state(conv,"completed").await?;h.idle(conv).await?;
  if harness=="claude" {
   h.submit(&user,conv,message("M3_QUESTION")).await?;let run=h.wait_state(conv,"requires_action").await?;let (action,epoch)=h.action(run).await?;
   h.submit(&user,conv,Command::AnswerQuestion(AnswerQuestion{action_id:action.to_string(),run_epoch:epoch as u64,answer:"Blue".into()})).await?;h.wait_state(conv,"completed").await?;h.idle(conv).await?;
  }
  let latest:Uuid=sqlx::query_scalar("SELECT id FROM items WHERE conversation_id=$1 AND kind='assistant' AND state='completed' ORDER BY last_seq DESC LIMIT 1").bind(conv).fetch_one(&h.db).await?;
  let fork=h.submit(&user,conv,Command::Fork(Fork{item_id:latest.to_string()})).await?;let fork=h.fork_result(fork).await?;
  h.submit(&user,fork,message("hello fork")).await?;h.wait_state(fork,"completed").await?;h.idle(fork).await?;
  h.submit(&user,conv,Command::Compact(Compact{})).await?;h.wait_state(conv,"completed").await?;h.idle(conv).await?;
  let job=h.json(&user,&format!("/orgs/{org}/conversations/{conv}/workspace"),reqwest::Method::POST,json!({"kind":"restore"})).await?;let job=id(&job,"id");
  for _ in 0..1000 {let state:String=sqlx::query_scalar("SELECT state FROM workspace_jobs WHERE id=$1").bind(job).fetch_one(&h.db).await?;if state=="completed"{break;}ensure!(state!="failed","restore failed");tokio::time::sleep(Duration::from_millis(100)).await;}
  h.submit(&user,conv,message("hello restored")).await?;h.wait_state(conv,"completed").await?;h.idle(conv).await?;
  let (sandbox,epoch):(Uuid,i64)=sqlx::query_as("SELECT id,exec_epoch FROM sandboxes WHERE conversation_id=$1 AND state='running'").bind(conv).fetch_one(&h.db).await?;
  let check=guest_exec(std::path::Path::new(&h.app.config.sandbox.host_socket),sandbox,epoch,"project-read-only","printf bad > /project/nope").await?;ensure!(check["exit_code"]!=0,"/project must be read-only");
  h.submit(&user,conv,message("slow cancel")).await?;h.wait_state(conv,"running").await?;
  h.submit(&user,conv,Command::Cancel(Cancel{})).await?;h.wait_state(conv,"cancelled").await?;
  h.idle(conv).await?;
  let project=h.json(&user,&format!("/orgs/{org}/projects"),reqwest::Method::POST,json!({"name":"Work retention","memory_mode":"default"})).await?;let project=id(&project,"id");
  h.json(&user,&format!("/orgs/{org}/conversations/{conv}/project"),reqwest::Method::POST,json!({"project_id":project})).await?;
  h.json(&user,&format!("/orgs/{org}/projects/{project}"),reqwest::Method::DELETE,json!({})).await?;
  sqlx::query("UPDATE projects SET deleted_at=now()-interval '31 days' WHERE id=$1").bind(project).execute(&h.db).await?;
  ensure!(ryzhub_server::projects::collect(&h.app,org,project).await?,"Work project expiry must reclaim its sandbox and checkpoints");
  let live:i64=sqlx::query_scalar("SELECT count(*) FROM checkpoints WHERE conversation_id=$1 AND state<>'deleted'").bind(conv).fetch_one(&h.db).await?;ensure!(live==0);

 }
 Ok::<_,anyhow::Error>(())
 }.await;
    owner.kill().await?;
    h.close().await?;
    outcome
}

#[path = "m3/regressions.rs"]
mod regressions;
