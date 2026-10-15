//! Real PostgreSQL, NATS and containerd; only the paid model upstream is a protocol fixture.
mod common;
use anyhow::{Context, ensure};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::HeaderMap,
    routing::post,
};
use prost::Message;
use ryzhub_proto::{command_envelope::Command, *};
use ryzhub_server::{
    app::App,
    config::{Config, ModelRoute, Upstream},
    http::router,
    llm::{gateway, keys},
    owner,
    store::{Lease, Store, hash_token},
};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
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
/// A 1×1 PNG, small enough to reach the model unchanged.
const DOT_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
async fn upstream(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    requests.lock().unwrap().push((body.clone(), headers));
    let last = body["messages"].as_array().unwrap().last().unwrap()["content"].clone();
    // A message carrying files is a list of parts; its text parts make the prompt.
    let prompt = match &last {
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.as_str().unwrap_or("").to_owned(),
    };
    let prompt = prompt.as_str();
    let result = last
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"));
    // Re-sent assistant content continues a paused turn.
    let continued = body["messages"].as_array().unwrap().last().unwrap()["role"] == "assistant";
    // What the member last asked, past the tool results that followed it.
    let asked = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .find(|content| !content.contains("tool_result"))
        .unwrap_or_default();
    use axum::response::IntoResponse;
    // Compaction on demand: a summary in place of the context, unless it says "nocompact".
    if body.get("compaction").is_some() {
        if body["messages"].to_string().contains("nocompact") {
            return axum::response::Response::builder()
                .status(400)
                .header("content-type", "application/json")
                .body(Body::from(json!({"type":"error","error":{"type":"invalid_request_error","message":"compaction unavailable"}}).to_string()))
                .unwrap();
        }
        return Json(json!({"id":"fixture-compaction","type":"message","role":"assistant","model":"fixture-model",
            "content":[{"type":"compaction","content":"压缩摘要"}],"stop_reason":"compaction",
            "usage":{"input_tokens":0,"output_tokens":0,"iterations":[{"type":"compaction","input_tokens":40,"output_tokens":6}]}}))
        .into_response();
    }
    // The summary a context falls back to.
    if body["stream"] != true && prompt.starts_with("Summarize our conversation") {
        return Json(json!({"id":"fixture-summary","type":"message","role":"assistant","model":"fixture-model",
            "content":[{"type":"text","text":"摘要内容"}],"stop_reason":"end_turn","usage":{"input_tokens":30,"output_tokens":4}}))
        .into_response();
    }
    let mut content = vec![
        json!({"type":"thinking","thinking":"opaque reasoning","signature":"signed-block"}),
        json!({"type":"redacted_thinking","data":"opaque-data"}),
    ];
    let mut paused = false;
    if continued {
        content.push(json!({"type":"text","text":"搜索结果显示 RyzHub 已发布","citations":[
            {"type":"web_search_result_location","url":"https://example.com/a","title":"Example A","encrypted_index":"opaque-index","cited_text":"RyzHub released"},
            {"type":"web_search_result_location","url":"https://example.com/a","title":"Example A","encrypted_index":"opaque-index-2","cited_text":"again"}
        ]}));
    } else if !result && body.get("tools").is_some() && prompt.contains("websearch") {
        content.push(json!({"type":"text","text":"先搜索一下"}));
        content.push(json!({"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"RyzHub"}}));
        content.push(json!({"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[
            {"type":"web_search_result","url":"https://example.com/a","title":"Example A","encrypted_content":"opaque-search","page_age":null}
        ]}));
        paused = true;
    } else if !result && body.get("tools").is_some() && prompt.contains("sharefiles") {
        let cmd = "mkdir -p out && printf 'quarterly report' > out/report.txt && ln -sf /etc/passwd leak && echo made";
        content.push(json!({"type":"tool_use","id":"call_exec","name":"exec","input":{"cmd":cmd,"timeout_ms":10000}}));
    } else if !result && body.get("tools").is_some() && prompt.contains("sharecheck") {
        content.push(json!({"type":"tool_use","id":"call_exec","name":"exec","input":{"cmd":"cat /attachments/report.txt","timeout_ms":10000}}));
    } else if result && asked.contains("sharefiles") {
        content.push(json!({"type":"text","text":"文件在这里：[报告](sandbox:/workspace/out/report.txt)、[缺失](sandbox:/workspace/missing.txt)、[泄露](sandbox:/workspace/leak)、[目录](sandbox:/workspace/out)，代码里的 `[不算](sandbox:/workspace/out/report.txt)` 不算。"}));
    } else if result && asked.contains("sharecheck") {
        content.push(json!({"type":"text","text":"还是这份：[报告](sandbox:/workspace/out/report.txt)，也就是 [附件里的](/attachments/report.txt)。"}));
    } else if !result && body.get("tools").is_some() && prompt.contains("attachfiles") {
        let cmd = "cat /attachments/notes.csv; ls /attachments; touch /attachments/new 2>/dev/null || echo READONLY";
        content.push(json!({"type":"tool_use","id":"call_exec","name":"exec","input":{"cmd":cmd,"timeout_ms":10000}}));
    } else if !result && body.get("tools").is_some() && prompt.contains("fscheck") {
        content.push(json!({"type":"tool_use","id":"call_a","name":"fs_upload","input":{"name":"notes.csv","path":"data/notes.csv"}}));
        content.push(json!({"type":"tool_use","id":"call_b","name":"exec","input":{"cmd":"cat data/notes.csv; printf 'made here' > out.txt","timeout_ms":10000}}));
        content.push(json!({"type":"tool_use","id":"call_c","name":"fs_download","input":{"path":"out.txt"}}));
    } else if !result && body.get("tools").is_some() && prompt.contains("depscheck") {
        content.push(json!({"type":"tool_use","id":"call_exec","name":"exec","input":{"cmd":"cat /opt/ryzhub-deps/marker","timeout_ms":10000}}));
    } else if !result && body.get("tools").is_some() && prompt.contains("markercat") {
        content.push(json!({"type":"tool_use","id":"call_exec","name":"exec","input":{"cmd":"printf once >> marker; cat marker","timeout_ms":10000}}));
    } else if !result && body.get("tools").is_some() && prompt.contains("quotacheck") {
        let cmd = "for f in /workspace/big /root/big; do dd if=/dev/zero of=$f bs=1M count=300 2>&1 | grep -oE 'No space left on device|Disk quota exceeded' | head -1; done; rm -f /workspace/big /root/big; awk 'BEGIN{s=\"x\"; while(1) s=s s}'; echo HOG=$?; echo small > /workspace/ok && echo SMALL_OK";
        content.push(json!({"type":"tool_use","id":"call_exec","name":"exec","input":{"cmd":cmd,"timeout_ms":60000}}));
    } else if !result && body.get("tools").is_some() && prompt.contains("egresscheck") {
        let cmd = "apk update >/tmp/apk.log 2>&1 && echo APK_OK; wget -q -T 10 -O /dev/null http://example.com/; echo WGET=$?; wget -Y off -q -T 5 -O /dev/null http://1.1.1.1/; echo DIRECT=$?; printenv https_proxy";
        content.push(json!({"type":"tool_use","id":"call_exec","name":"exec","input":{"cmd":cmd,"timeout_ms":60000}}));
    } else if !result && body.get("tools").is_some() && prompt.contains("streamcheck") {
        let cmd = "for i in 1 2 3; do echo line$i; sleep 1; done";
        content.push(json!({"type":"tool_use","id":"call_exec","name":"exec","input":{"cmd":cmd,"timeout_ms":20000}}));
    } else if !result && body.get("tools").is_some() && prompt.contains("attachls") {
        content.push(json!({"type":"tool_use","id":"call_exec","name":"exec","input":{"cmd":"ls /attachments","timeout_ms":10000}}));
    } else if !result && body.get("tools").is_some() && prompt.contains("exec") {
        let command = if prompt.contains("proxyexec") {
            r#"sleep 2; wget -Y off -qO- --header 'Content-Type: application/json' --post-data '{"model":"fixture-model","max_tokens":32,"stream":true,"messages":[{"role":"user","content":"guest local proxy"}]}' http://127.0.0.1:8090/v1/messages"#
        } else {
            "printf once >> marker; sleep 1; printf '真实沙箱结果'"
        };
        content.push(json!({"type":"text","text":"准备执行工具"}));
        content.push(json!({"type":"tool_use","id":"call_exec","name":"exec","input":{"cmd":command,"timeout_ms":10000}}));
    } else if !result && body.get("tools").is_some() && prompt.contains("viewimage") {
        let cmd = format!("printf %s {DOT_PNG} | base64 -d > dot.png");
        content.push(json!({"type":"tool_use","id":"call_a","name":"exec","input":{"cmd":cmd,"timeout_ms":10000}}));
        content.push(
            json!({"type":"tool_use","id":"call_b","name":"view_image","input":{"path":"dot.png"}}),
        );
    } else if !result && body.get("tools").is_some() && prompt.contains("bigoutput") {
        let cmd = "head -c 6000 /dev/zero | tr '\\0' a";
        content.push(json!({"type":"tool_use","id":format!("call_big_{}",Uuid::now_v7().simple()),"name":"exec","input":{"cmd":cmd,"timeout_ms":10000}}));
    } else if !result && body.get("tools").is_some() && prompt.contains("readback") {
        // A result moved out of the context, read back by the id its placeholder names.
        let moved = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|b| b["type"] == "tool_result")
            .find(|b| {
                b["content"]
                    .as_str()
                    .is_some_and(|c| c.starts_with("[Moved out"))
            })
            .map(|b| b["tool_use_id"].clone());
        content.push(json!({"type":"tool_use","id":"call_read","name":"read_tool_output","input":{"id":moved}}));
    } else if !result && prompt.contains("plugin") && body.get("tools").is_some() {
        // Plugin tools, by the word after "plugin".
        let (tool, input) = if prompt.contains("pluginlookup") {
            ("acme-tracker__lookup", json!({"id": "7"}))
        } else if prompt.contains("pluginask") {
            ("acme-tracker__ask", json!({}))
        } else if prompt.contains("pluginwidget") {
            ("acme-tracker__widget", json!({}))
        } else {
            (
                "load_skill",
                json!({"plugin": "acme-tracker", "name": "triage"}),
            )
        };
        content.push(json!({"type":"tool_use","id":format!("call_{}",Uuid::now_v7().simple()),"name":tool,"input":input}));
    } else if !result && prompt.contains("autocompact") {
        content.push(json!({"type":"compaction","content":"自动压缩摘要"}));
        content.push(json!({"type":"text","text":"压缩后继续"}));
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
            start = json!({"type":"text","text":""});
        }
        if block["type"] == "server_tool_use" {
            start["input"] = json!({});
        }
        if block["type"] == "compaction" {
            start["content"] = json!("");
        }
        events.push(json!({"type":"content_block_start","index":index,"content_block":start}));
        if block["type"] == "compaction" {
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"compaction_delta","content":block["content"]}}));
        }
        for citation in block["citations"].as_array().into_iter().flatten() {
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"citations_delta","citation":citation}}));
        }
        if block["type"] == "text" {
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":block["text"]}}));
        }
        if block["type"] == "server_tool_use" {
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":block["input"].to_string()}}));
        }
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    let stop = if paused {
        "pause_turn"
    } else if tool {
        "tool_use"
    } else {
        "end_turn"
    };
    events.push(
        json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":4}}),
    );
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
        Self::with(|_| {}).await
    }
    /// A harness whose configuration `configure` adjusts last.
    async fn with(configure: impl FnOnce(&mut Config)) -> anyhow::Result<Self> {
        let admin_url = std::env::var("TEST_ADMIN_URL").unwrap_or_else(|_| {
            "postgres://postgres:local-development-only@127.0.0.1:55432/postgres".into()
        });
        let admin = PgPool::connect(&admin_url).await?;
        common::sweep(
            &admin,
            admin_url.rsplit_once('/').unwrap().0,
            &common::host_socket(),
        )
        .await;
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
        config.llm.model = "fixture-model".into();
        config.llm.upstream_url = format!("http://{}", upstream_listener.local_addr()?);
        config.llm.gateway_url = format!("http://{}", gateway_listener.local_addr()?);
        config.llm.platform_key_env = "RYZHUB_TEST_UPSTREAM_KEY".into();
        config.llm.vault_key_env = "RYZHUB_TEST_VAULT_KEY".into();
        config.sandbox.enabled = true;
        config.sandbox.runtime = std::env::var("TEST_RUNTIME").unwrap_or_else(|_| "runc".into());
        // Sandboxes go when their run ends unless a test keeps them for reuse.
        config.sandbox.chat_idle_secs = 0;
        config.attachment_renderer = env!("CARGO_BIN_EXE_ryzhub-server").into();
        config.work.storage_root = format!("/tmp/{name}/blobs");
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
        // Plugin fixtures run on this machine; the app origin is served beside the API.
        let content_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        config.plugins.insecure_hosts = vec!["127.0.0.1".into()];
        config.public_url = format!("http://{}", api_listener.local_addr()?);
        config.plugins.client_url = "http://client.test".into();
        config.plugins.content_origin = format!("http://{}", content_listener.local_addr()?);
        configure(&mut config);
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
        let d = app.clone();
        let tasks = vec![
            tokio::spawn(async move {
                axum::serve(
                    content_listener,
                    ryzhub_server::plugins::apps::content_router(d),
                )
                .await
                .unwrap();
            }),
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
                        .route(
                            "/v1/messages/count_tokens",
                            post(|| async { Json(json!({"input_tokens":7})) }),
                        )
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
            .create_conversation(&ctx, "Platform", "chat")
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
                "test-device",
                &CommandEnvelope {
                    cid: cid.to_string(),
                    command: Some(command),
                },
            )
            .await?;
        Ok(cid)
    }
    /// The command's outcome once the Owner has processed it.
    async fn outcome(&self, cid: Uuid) -> anyhow::Result<frame::Body> {
        for _ in 0..400 {
            let result: Option<Vec<u8>> =
                sqlx::query_scalar("SELECT result FROM commands WHERE cid=$1")
                    .bind(cid)
                    .fetch_one(&self.db)
                    .await?;
            if let Some(result) = result {
                return Frame::decode(result.as_slice())?
                    .body
                    .context("empty frame");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("command was not processed")
    }
    /// Upload `body` as `name` to the user's file library.
    async fn upload(&self, user: &Value, name: &str, body: Vec<u8>) -> anyhow::Result<Value> {
        let org = id(user, "org_id");
        let response = reqwest::Client::new()
            .post(format!("{}/orgs/{org}/attachments?name={name}", self.base))
            .bearer_auth(user["token"].as_str().unwrap())
            .body(body)
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "upload: {}",
            response.status()
        );
        Ok(response.json().await?)
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
        for _ in 0..400 {
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
    /// The run after `previous` once it completes.
    async fn wait_after(&self, conv: Uuid, previous: Uuid) -> anyhow::Result<Uuid> {
        for _ in 0..400 {
            let run = self.wait_state(conv, "completed").await;
            if let Ok(run) = run
                && run != previous
            {
                return Ok(run);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("no run completed after {previous}")
    }
    async fn action(&self, run: Uuid) -> anyhow::Result<(Uuid, i64)> {
        Ok(sqlx::query_as(
            "SELECT id,run_epoch FROM pending_actions WHERE run_id=$1 AND status='pending'",
        )
        .bind(run)
        .fetch_one(&self.db)
        .await?)
    }
    /// A gateway key for one Owner model call, as the chat engine issues it.
    async fn loop_key(&self, lease: &Lease, user: Uuid, run: Uuid) -> anyhow::Result<String> {
        keys::issue_loop_key(
            &self.app.store,
            lease,
            user,
            run,
            vec![self.app.config.llm.model.clone()],
        )
        .await
    }
    async fn close(self) -> anyhow::Result<()> {
        common::release(
            &self.db,
            std::path::Path::new(&self.app.config.sandbox.host_socket),
        )
        .await;
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

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and test gateway secrets"]
async fn gateway_native_protocol_byok_usage_and_execution_fencing() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    let uid = id(&user, "user_id");
    h.send(&user, conv, message("hello")).await?;
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
    let token = h.loop_key(&lease, uid, run).await?;
    let client = reqwest::Client::new();
    let body = json!({"model":"fixture-model","max_tokens":32,"stream":true,"messages":[{"role":"user","content":"hello"}],"system":[{"type":"text","text":"native","cache_control":{"type":"ephemeral"}}]});
    let response = client
        .post(format!("{}/v1/messages", h.gateway))
        .bearer_auth(&token)
        .header("anthropic-beta", "future-feature-2026")
        .header("x-api-key", "never-forward-this")
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert!(response.contains("event: ping") && response.contains("signed-block"));
    let received = h.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!(received.0, body);
    assert_eq!(received.1["anthropic-beta"], "future-feature-2026");
    assert_eq!(received.1["x-api-key"], "local-fixture-key");
    assert!(received.1.get("authorization").is_none());
    let usage=sqlx::query("SELECT input_tokens,output_tokens,raw_usage,estimated,status FROM usage_records WHERE run_id=$1 ORDER BY created_at DESC LIMIT 1").bind(run).fetch_one(&h.db).await?;
    assert_eq!(usage.get::<i64, _>("input_tokens"), 25);
    assert_eq!(usage.get::<i64, _>("output_tokens"), 8);
    assert!(!usage.get::<bool, _>("estimated"));
    assert_eq!(
        usage.get::<Value, _>("raw_usage")["cache_read_input_tokens"],
        12
    );
    let credential: Value = client
        .post(format!("{}/orgs/{org}/credentials", h.base))
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"name":"personal","api_key":"personal-fixture-key"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    // The key lives envelope-encrypted in a secret; the credential row holds only its id.
    let (legacy, ciphertext, wrapped): (Option<Vec<u8>>, Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT c.encrypted_key,s.ciphertext,s.wrapped_dek FROM model_credentials c
         JOIN secrets s ON s.org_id=c.org_id AND s.id=c.secret_id WHERE c.id=$1",
    )
    .bind(id(&credential, "id"))
    .fetch_one(&h.db)
    .await?;
    assert!(legacy.is_none());
    for stored in [&ciphertext, &wrapped] {
        assert!(!stored.windows(20).any(|b| b == b"personal-fixture-key"));
    }
    client
        .post(format!("{}/v1/messages", h.gateway))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    assert_eq!(
        h.requests.lock().unwrap().last().unwrap().1["x-api-key"],
        "personal-fixture-key"
    );
    sqlx::query("UPDATE runs SET exec_epoch=exec_epoch+1 WHERE id=$1")
        .bind(run)
        .execute(&h.db)
        .await?;
    assert_eq!(
        client
            .post(format!("{}/v1/messages", h.gateway))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .status(),
        409
    );
    assert_eq!(
        client
            .post(format!("{}/v1/messages", h.gateway))
            .bearer_auth(format!("{token}x"))
            .json(&body)
            .send()
            .await?
            .status(),
        401
    );
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and test gateway secrets"]
async fn loop_question_checkpoint_steer_and_read_only_side_query() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut owner = h.owner()?;
    h.send(&user, conv, message("question")).await?;
    let run = h.wait_state(conv, "requires_action").await?;
    let (action, epoch) = h.action(run).await?;
    h.send(
        &user,
        conv,
        Command::AnswerQuestion(AnswerQuestion {
            action_id: action.to_string(),
            run_epoch: epoch as u64,
            answer: "绿色".into(),
        }),
    )
    .await?;
    h.wait_state(conv, "completed").await?;
    let steps: Vec<Value> = sqlx::query_scalar(
        "SELECT native_response FROM loop_steps WHERE run_id=$1 ORDER BY step_no",
    )
    .bind(run)
    .fetch_all(&h.db)
    .await?;
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["content"][0]["signature"], "signed-block");
    let tool_item: (String, String) = sqlx::query_as("SELECT i.state,i.text FROM tool_calls t JOIN items i ON i.id=t.item_id AND i.org_id=t.org_id WHERE t.run_id=$1").bind(run).fetch_one(&h.db).await?;
    assert_eq!(tool_item.0, "completed");
    assert!(tool_item.1.contains("绿色"));
    assert!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .any(|(r, _)| r["messages"].to_string().contains("绿色")
                && r["messages"].to_string().contains("opaque-data"))
    );
    let conv2 = h.conversation(&user).await?;
    h.send(&user, conv2, message("slow")).await?;
    h.wait_state(conv2, "running").await?;
    h.send(
        &user,
        conv2,
        Command::Steer(Steer {
            text: "补充输入".into(),
            ..Default::default()
        }),
    )
    .await?;
    h.wait_state(conv2, "completed").await?;
    let consumed:bool=sqlx::query_scalar("SELECT bool_and(consumed_step IS NOT NULL) FROM pending_inputs WHERE run_id IN (SELECT id FROM runs WHERE conversation_id=$1)").bind(conv2).fetch_one(&h.db).await?;
    assert!(consumed);
    h.send(
        &user,
        conv2,
        Command::SideQuery(SideQuery {
            text: "旁问".into(),
        }),
    )
    .await?;
    // Wait for the new command to create its own run, rather than observing the previous completed run.
    for _ in 0..200 {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM runs WHERE conversation_id=$1 AND read_only")
                .bind(conv2)
                .fetch_one(&h.db)
                .await?;
        if count > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let side = h.wait_state(conv2, "completed").await?;
    let side_step: Value =
        sqlx::query_scalar("SELECT request_messages FROM loop_steps WHERE run_id=$1 LIMIT 1")
            .bind(side)
            .fetch_one(&h.db)
            .await?;
    assert!(side_step.to_string().contains("旁问"));
    assert!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .any(|(r, _)| r.get("tools").is_none())
    );
    let cid = h.send(&user, conv2, message("after-side")).await?;
    for _ in 0..200 {
        let done: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE cause_cid=$1 AND state='completed')",
        )
        .bind(cid)
        .fetch_one(&h.db)
        .await?;
        if done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let seed: Value = sqlx::query_scalar(
        "SELECT seed_messages FROM runs WHERE cause_cid=$1 AND state='completed'",
    )
    .bind(cid)
    .fetch_one(&h.db)
    .await?;
    assert!(!seed.to_string().contains("旁问"));
    let paused = h.conversation(&user).await?;
    h.send(&user, paused, message("question")).await?;
    h.wait_state(paused, "requires_action").await?;
    h.send(
        &user,
        paused,
        Command::Steer(Steer {
            text: "不会被采用的补充".into(),
            ..Default::default()
        }),
    )
    .await?;
    h.send(&user, paused, Command::Cancel(Cancel {})).await?;
    h.wait_state(paused, "cancelled").await?;
    let state: String =
        sqlx::query_scalar("SELECT state FROM items WHERE conversation_id=$1 AND kind='steer'")
            .bind(paused)
            .fetch_one(&h.db)
            .await?;
    assert_eq!(state, "cancelled");
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and test gateway secrets"]
async fn paused_web_search_turn_resumes_before_steer_and_keeps_citations() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut owner = h.owner()?;
    h.send(&user, conv, message("websearch slow")).await?;
    // The steer must arrive while the paused step streams, after it took its inputs.
    for _ in 0..400 {
        let started = h
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(r, _)| r["messages"].to_string().contains("websearch slow"));
        if started {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    h.send(
        &user,
        conv,
        Command::Steer(Steer {
            text: "补充输入".into(),
            ..Default::default()
        }),
    )
    .await?;
    let run = h.wait_state(conv, "completed").await?;
    let requests: Vec<Value> = h
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|(r, _)| r.clone())
        .filter(|r| r["messages"].to_string().contains("websearch slow"))
        .collect();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert!(
            request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["type"] == "web_search_20260318" && t["name"] == "web_search")
        );
    }
    // The paused turn is re-sent as is, without the steer that arrived meanwhile.
    let resumed = requests[1]["messages"].as_array().unwrap();
    let paused = resumed.last().unwrap();
    assert_eq!(paused["role"], "assistant");
    assert!(!paused.to_string().contains("补充输入"));
    let blocks = paused["content"].as_array().unwrap();
    assert!(
        blocks
            .iter()
            .any(|b| b["type"] == "server_tool_use" && b["input"]["query"] == "RyzHub")
    );
    assert!(blocks.iter().any(|b| b["type"] == "web_search_tool_result"
        && b["content"][0]["encrypted_content"] == "opaque-search"));
    // The steer follows the finished turn, which is one assistant message.
    let after = requests[2]["messages"].as_array().unwrap();
    assert_eq!(after.last().unwrap()["content"], "补充输入");
    let turn = &after[after.len() - 2];
    assert_eq!(turn["role"], "assistant");
    assert_eq!(after[after.len() - 3]["role"], "user");
    let turn = turn.to_string();
    assert!(turn.contains("先搜索一下") && turn.contains("opaque-index"));
    // The search splits the paused step's reply where it happened.
    let items: Vec<(String, String, String, Option<Value>)> = sqlx::query_as(
        "SELECT kind,state,text,detail FROM items WHERE run_id=$1 AND kind IN ('assistant','tool')
         ORDER BY first_seq",
    )
    .bind(run)
    .fetch_all(&h.db)
    .await?;
    let shape: Vec<(&str, &str, &str)> = items
        .iter()
        .map(|(kind, state, text, _)| (kind.as_str(), state.as_str(), text.as_str()))
        .collect();
    assert_eq!(shape[0], ("assistant", "completed", "先搜索一下"));
    assert_eq!((shape[1].0, shape[1].1), ("tool", "completed"));
    assert_eq!(shape[2], ("assistant", "completed", ""));
    assert_eq!(
        shape[3],
        (
            "assistant",
            "completed",
            "搜索结果显示 RyzHub 已发布 ([example.com](https://example.com/a))"
        )
    );
    assert_eq!(
        shape[4],
        ("assistant", "completed", "本地协议联调：你好 Rust")
    );
    assert_eq!(shape.len(), 5);
    let search = &items[1].3.as_ref().unwrap()["kind"]["Tool"];
    assert_eq!(search["name"], "web_search");
    assert_eq!(search["arguments_json"], r#"{"query":"RyzHub"}"#);
    let result: Value = serde_json::from_str(search["result_json"].as_str().unwrap())?;
    assert_eq!(
        result,
        json!({"matches":[{"url":"https://example.com/a","title":"Example A"}]})
    );
    let details: Vec<&Value> = items.iter().filter_map(|i| i.3.as_ref()).collect();
    assert_eq!(details.len(), 2);
    let citations = details[1]["kind"]["Answer"]["citations"]
        .as_array()
        .unwrap();
    assert_eq!(citations.len(), 1);
    assert_eq!(citations[0]["url"], "https://example.com/a");
    assert_eq!(citations[0]["title"], "Example A");
    assert_eq!(citations[0]["cited_text"], "RyzHub released");
    // The run's end is announced with how long it took.
    let payloads: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT payload FROM events WHERE kind='run_state_changed'")
            .fetch_all(&h.db)
            .await?;
    let ended = payloads
        .iter()
        .filter_map(|p| match EventEnvelope::decode(p.as_slice()).ok()?.event? {
            event_envelope::Event::RunStateChanged(state) => Some(state),
            _ => None,
        })
        .find(|state| state.run_id == run.to_string() && state.state == "completed")
        .unwrap();
    assert!(ended.started_at_ms > 0 && ended.ended_at_ms >= ended.started_at_ms);
    // The cited page showed while the reply was still streaming, with its link in the text.
    let payloads: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT payload FROM events WHERE kind='item_upserted'")
            .fetch_all(&h.db)
            .await?;
    let live = payloads
        .iter()
        .filter_map(|p| match EventEnvelope::decode(p.as_slice()).ok()?.event? {
            event_envelope::Event::ItemUpserted(item) => Some(item),
            _ => None,
        })
        .find(|item| {
            item.state == "streaming"
                && item.run_id == run.to_string()
                && matches!(
                    item.detail.as_ref().and_then(|d| d.kind.as_ref()),
                    Some(item_detail::Kind::Answer(answer)) if answer.citations.len() == 1
                )
        })
        .unwrap();
    assert!(
        live.text
            .ends_with(" ([example.com](https://example.com/a))")
    );
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and test gateway secrets"]
async fn retried_step_hides_what_its_failed_searches_split() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut owner = h.owner()?;
    // Every attempt searches, then loses the end of its stream.
    h.send(&user, conv, message("websearch interrupt")).await?;
    let run = h.wait_state(conv, "failed").await?;
    let items: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT kind,state,text FROM items WHERE run_id=$1 AND kind IN ('assistant','tool')
         ORDER BY first_seq",
    )
    .bind(run)
    .fetch_all(&h.db)
    .await?;
    let shape: Vec<(&str, &str)> = items
        .iter()
        .map(|(kind, state, _)| (kind.as_str(), state.as_str()))
        .collect();
    // The search item is shared by the attempts; each attempt's continuation is its own.
    assert_eq!(
        shape,
        [
            ("assistant", "completed"),
            ("tool", "completed"),
            ("assistant", "discarded"),
            ("assistant", "discarded"),
            ("assistant", "failed"),
        ]
    );
    assert_eq!(items[0].2, "先搜索一下");
    assert!(items[2].2.is_empty() && items[3].2.is_empty());
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and test gateway secrets"]
async fn a_conversation_held_by_a_lock_does_not_hold_up_others() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let (held, free) = (h.conversation(&user).await?, h.conversation(&user).await?);
    // Someone holds the first conversation's channel row, as a long cleanup would.
    let mut lock = h.db.begin().await?;
    sqlx::query("SELECT 1 FROM channels WHERE conversation_id=$1 FOR UPDATE")
        .bind(held)
        .execute(&mut *lock)
        .await?;
    let mut owner = h.owner()?;
    let waiting = h.send(&user, held, message("hello held")).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = std::time::Instant::now();
    let other = h.send(&user, free, message("hello free")).await?;
    h.outcome(other).await?;
    ensure!(
        started.elapsed() < Duration::from_secs(5),
        "the other conversation waited {:?} behind the lock",
        started.elapsed()
    );
    // Once the row is free, the first conversation is claimed too.
    lock.rollback().await?;
    h.outcome(waiting).await?;
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS, secrets and scripts/start-execution-node.sh"]
async fn replies_hand_over_the_sandbox_files_they_link_to() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    ensure!(
        !h.app.config.sandbox.image.is_empty(),
        "execution image missing"
    );
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let token = user["token"].as_str().unwrap().to_owned();
    let mut owner = h.owner()?;
    let conv = h.conversation(&user).await?;
    h.send(&user, conv, message("sharefiles 做个报告")).await?;
    let run = h.wait_state(conv, "completed").await?;
    // Models are told how.
    assert!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .any(|(body, _)| body["system"]
                .to_string()
                .contains("sandbox:/workspace/report.pdf")),
        "no request explained file links"
    );
    let reply = |run: Uuid| {
        let db = h.db.clone();
        async move {
            let detail: Value = sqlx::query_scalar(
                "SELECT detail FROM items WHERE run_id=$1 AND kind='assistant' AND state='completed'
                 AND text LIKE '%sandbox:%' ORDER BY first_seq DESC LIMIT 1",
            )
            .bind(run)
            .fetch_one(&db)
            .await?;
            anyhow::Ok(
                detail["kind"]["Answer"]["files"]
                    .as_array()
                    .unwrap()
                    .clone(),
            )
        }
    };
    let files = reply(run).await?;
    // In order, each once; the link inside code is not one.
    let links: Vec<&str> = files.iter().map(|f| f["link"].as_str().unwrap()).collect();
    assert_eq!(
        links,
        [
            "/workspace/out/report.txt",
            "/workspace/missing.txt",
            "/workspace/leak",
            "/workspace/out"
        ]
    );
    let report = &files[0]["file"];
    assert_eq!(report["name"], "report.txt");
    assert_eq!(report["path"], "report.txt");
    assert_eq!(report["kind"], "text");
    assert_eq!(report["size"], 16);
    assert_eq!(files[1]["error"], "FILE_NOT_FOUND");
    // The link resolves inside the workspace, where there is no /etc/passwd.
    assert_eq!(files[2]["error"], "FILE_NOT_FOUND");
    assert!(files[2]["file"].is_null());
    assert_eq!(files[3]["error"], "NOT_A_FILE");
    // It is in the owner's library as a published file, and reads back as written.
    let http = reqwest::Client::new();
    let library: Value = http
        .get(format!(
            "{}/orgs/{org}/attachments?direction=output",
            h.base
        ))
        .bearer_auth(&token)
        .send()
        .await?
        .json()
        .await?;
    let listed = library["files"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], report["id"]);
    assert_eq!(listed[0]["direction"], "output");
    let content = http
        .get(format!(
            "{}/orgs/{org}/attachments/{}/content",
            h.base,
            report["id"].as_str().unwrap()
        ))
        .bearer_auth(&token)
        .send()
        .await?
        .bytes()
        .await?;
    assert_eq!(&content[..], b"quarterly report");

    // The next run's sandbox has it under /attachments; linking it again hands over the same
    // entry, by either path.
    h.send(&user, conv, message("sharecheck 再看看")).await?;
    let mut second = run;
    for _ in 0..200 {
        second = h.wait_state(conv, "completed").await?;
        if second != run {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ensure!(second != run, "the second run did not complete");
    let result: Value =
        sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1 AND call_id='call_exec'")
            .bind(second)
            .fetch_one(&h.db)
            .await?;
    assert!(
        result["stdout_head"]
            .as_str()
            .unwrap_or_default()
            .contains("quarterly report"),
        "{result}"
    );
    let again = reply(second).await?;
    assert_eq!(again.len(), 2);
    assert_eq!(again[0]["file"]["id"], report["id"]);
    assert_eq!(again[1]["link"], "/attachments/report.txt");
    assert_eq!(again[1]["file"]["id"], report["id"]);
    let library: Value = http
        .get(format!(
            "{}/orgs/{org}/attachments?direction=output",
            h.base
        ))
        .bearer_auth(&token)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(library["files"].as_array().unwrap().len(), 1);
    owner.kill().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS, secrets and scripts/start-execution-node.sh"]
async fn library_files_reach_models_and_sandboxes_until_deleted() -> anyhow::Result<()> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let h = Harness::new().await?;
    ensure!(
        !h.app.config.sandbox.image.is_empty(),
        "execution image missing"
    );
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let token = user["token"].as_str().unwrap().to_owned();
    let png = b64.decode(DOT_PNG)?;
    let pdf = b"%PDF-1.4\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n".to_vec();
    let image = h.upload(&user, "dot.png", png.clone()).await?;
    let document = h.upload(&user, "brief.pdf", pdf.clone()).await?;
    let text = h.upload(&user, "notes.csv", b"a,b\n1,2\n".to_vec()).await?;
    let binary = h.upload(&user, "data.bin", vec![0, 1, 2, 3]).await?;
    let kinds: Vec<&str> = [&image, &document, &text, &binary]
        .iter()
        .map(|file| file["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["image", "pdf", "text", "file"]);
    assert_eq!(image["preview"], true);
    assert_eq!(
        (image["width"].as_u64(), image["height"].as_u64()),
        (Some(1), Some(1))
    );
    let http = reqwest::Client::new();
    let get = |path: String, token: String| {
        let http = http.clone();
        let url = format!("{}{path}", h.base);
        async move { http.get(url).bearer_auth(token).send().await }
    };
    let image_id = image["id"].as_str().unwrap().to_owned();
    let content = get(
        format!("/orgs/{org}/attachments/{image_id}/content"),
        token.clone(),
    )
    .await?;
    assert_eq!(
        content.headers()["content-type"],
        "application/octet-stream"
    );
    assert!(
        content.headers()["content-disposition"]
            .to_str()?
            .starts_with("attachment;")
    );
    assert_eq!(content.bytes().await?.to_vec(), png);
    let preview = get(
        format!("/orgs/{org}/attachments/{image_id}/preview"),
        token.clone(),
    )
    .await?;
    assert_eq!(preview.headers()["content-type"], "image/png");
    // Another member's library is out of reach, to read or to send.
    let stranger = h.user().await?;
    let elsewhere = format!(
        "/orgs/{}/attachments/{image_id}/content",
        id(&stranger, "org_id")
    );
    assert!(
        !get(elsewhere, stranger["token"].as_str().unwrap().into())
            .await?
            .status()
            .is_success()
    );
    let mut owner = h.owner()?;
    let theirs = h.conversation(&stranger).await?;
    let cid = h
        .send(
            &stranger,
            theirs,
            Command::SendMessage(SendMessage {
                text: "borrow".into(),
                attachment_ids: vec![image_id.clone()],
                ..Default::default()
            }),
        )
        .await?;
    match h.outcome(cid).await? {
        frame::Body::CommandRejected(rejected) => {
            assert_eq!(rejected.code, "ATTACHMENT_UNAVAILABLE")
        }
        other => anyhow::bail!("expected a rejection: {other:?}"),
    }

    let conv = h.conversation(&user).await?;
    let ids: Vec<String> = [&image, &document, &text, &binary]
        .iter()
        .map(|file| file["id"].as_str().unwrap().to_owned())
        .collect();
    h.send(
        &user,
        conv,
        Command::SendMessage(SendMessage {
            text: "attachfiles 看看".into(),
            attachment_ids: ids,
            ..Default::default()
        }),
    )
    .await?;
    let run = h.wait_state(conv, "completed").await?;
    let first = h
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|(body, _)| body.clone())
        .find(|body| body.to_string().contains("\"document\""))
        .context("no request carried the files")?;
    let parts = first["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user" && message["content"][0]["type"] == "image")
        .context("user message with files")?["content"]
        .clone();
    assert_eq!(
        parts[0],
        json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":DOT_PNG}})
    );
    assert_eq!(parts[1]["type"], "document");
    assert_eq!(parts[1]["source"]["data"], b64.encode(&pdf));
    let note = parts[2]["text"].as_str().unwrap();
    assert!(
        note.contains("/attachments/dot.png") && note.contains("/attachments/data.bin"),
        "{note}"
    );
    assert!(
        parts[3]["text"]
            .as_str()
            .unwrap()
            .contains("<attachment path=\"/attachments/notes.csv\">\na,b\n1,2\n"),
        "{}",
        parts[3]
    );
    assert_eq!(parts[4]["text"], "attachfiles 看看");
    let result: Value =
        sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1 AND call_id='call_exec'")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
    let stdout = result["stdout_head"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("a,b") && stdout.contains("dot.png") && stdout.contains("READONLY"),
        "{result}"
    );
    let detail: Value =
        sqlx::query_scalar("SELECT detail FROM items WHERE conversation_id=$1 AND kind='user'")
            .bind(conv)
            .fetch_one(&h.db)
            .await?;
    let files = detail["kind"]["Attachments"]["files"]
        .as_array()
        .context("attachments detail")?;
    assert_eq!(files.len(), 4);
    assert_eq!(
        (files[0]["path"].as_str(), files[0]["kind"].as_str()),
        (Some("dot.png"), Some("image"))
    );
    let copies: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM loop_steps s JOIN runs r ON r.id=s.run_id WHERE r.conversation_id=$1
         AND (coalesce(s.request_messages::text,'') LIKE '%'||$2||'%' OR coalesce(s.input_delta::text,'') LIKE '%'||$2||'%')",
    )
    .bind(conv)
    .bind(DOT_PNG)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(copies, 0, "model context must not store file bytes");

    // A fork keeps the files: its references, the message's detail and the model context.
    let latest: Uuid = sqlx::query_scalar(
        "SELECT id FROM items WHERE conversation_id=$1 AND kind='assistant' AND state='completed'
         ORDER BY last_seq DESC LIMIT 1",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    let cid = h
        .send(
            &user,
            conv,
            Command::Fork(Fork {
                item_id: latest.to_string(),
            }),
        )
        .await?;
    let mut fork = None;
    for _ in 0..400 {
        let row: Option<(String, Option<Value>)> =
            sqlx::query_as("SELECT state,result FROM conversation_operations WHERE id=$1")
                .bind(cid)
                .fetch_optional(&h.db)
                .await?;
        let Some((state, result)) = row else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        ensure!(state != "failed", "fork failed: {result:?}");
        if state == "completed" {
            fork = result.map(|result| id(&result, "conversation_id"));
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let fork = fork.context("fork did not complete")?;
    let references: i64 =
        sqlx::query_scalar("SELECT count(*) FROM conversation_files WHERE conversation_id=$1")
            .bind(fork)
            .fetch_one(&h.db)
            .await?;
    assert_eq!(references, 4);
    let copied: Value =
        sqlx::query_scalar("SELECT detail FROM items WHERE conversation_id=$1 AND kind='user'")
            .bind(fork)
            .fetch_one(&h.db)
            .await?;
    assert_eq!(
        copied["kind"]["Attachments"]["files"]
            .as_array()
            .map(Vec::len),
        Some(4)
    );
    h.send(&user, fork, message("forked follow-up")).await?;
    h.wait_state(fork, "completed").await?;
    assert!(
        h.requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .0
            .to_string()
            .contains(DOT_PNG)
    );

    // The same file in another conversation, with no text at all.
    let other = h.conversation(&user).await?;
    h.send(
        &user,
        other,
        Command::SendMessage(SendMessage {
            attachment_ids: vec![image_id.clone()],
            ..Default::default()
        }),
    )
    .await?;
    h.wait_state(other, "completed").await?;
    assert!(
        h.requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .0
            .to_string()
            .contains(DOT_PNG)
    );
    let library = get(format!("/orgs/{org}/attachments"), token.clone())
        .await?
        .json::<Value>()
        .await?;
    let listed = library["files"].as_array().unwrap();
    assert_eq!(listed.len(), 4);
    let entry = listed
        .iter()
        .find(|file| file["id"] == image["id"])
        .unwrap();
    // The first conversation, its fork and the second conversation.
    assert_eq!(entry["conversations"], 3);
    // The library is searched by name, ignoring case, and filtered by kind.
    let names = |path: String| {
        let get = &get;
        let token = token.clone();
        async move {
            let page = get(path, token).await?.json::<Value>().await?;
            anyhow::Ok(
                page["files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|file| file["name"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>(),
            )
        }
    };
    assert_eq!(
        names(format!("/orgs/{org}/attachments?q=NOTES")).await?,
        ["notes.csv"]
    );
    assert_eq!(
        names(format!("/orgs/{org}/attachments?kind=pdf,text")).await?,
        ["notes.csv", "brief.pdf"]
    );
    assert!(
        names(format!("/orgs/{org}/attachments?q=dot&kind=pdf"))
            .await?
            .is_empty()
    );
    assert_eq!(
        get(format!("/orgs/{org}/attachments?kind=video"), token.clone())
            .await?
            .status(),
        400
    );

    // A second entry with the same content, then deleting the first removes it everywhere.
    let copy = h.upload(&user, "dot-copy.png", png.clone()).await?;
    // Both are found by their content, the newest first, before anything is uploaded again.
    let content_hash = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(&png))
    };
    assert_eq!(copy["sha256"], content_hash.as_str());
    assert_eq!(
        names(format!("/orgs/{org}/attachments?sha256={content_hash}")).await?,
        ["dot-copy.png", "dot.png"]
    );
    assert_eq!(
        names(format!(
            "/orgs/{org}/attachments?sha256={}",
            content_hash.to_uppercase()
        ))
        .await?,
        ["dot-copy.png", "dot.png"]
    );
    assert_eq!(
        get(format!("/orgs/{org}/attachments?sha256=xyz"), token.clone())
            .await?
            .status(),
        400
    );
    let deleted = http
        .delete(format!("{}/orgs/{org}/attachments/{image_id}", h.base))
        .bearer_auth(&token)
        .send()
        .await?;
    assert!(deleted.status().is_success());
    let gone = get(
        format!("/orgs/{org}/attachments/{image_id}/preview"),
        token.clone(),
    )
    .await?;
    assert_eq!(gone.status(), 410);
    assert_eq!(
        names(format!("/orgs/{org}/attachments?sha256={content_hash}")).await?,
        ["dot-copy.png"]
    );
    h.send(&user, conv, message("attachls")).await?;
    let mut later = None;
    for _ in 0..400 {
        let runs: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM runs WHERE conversation_id=$1 AND state='completed' ORDER BY started_at",
        )
        .bind(conv)
        .fetch_all(&h.db)
        .await?;
        if runs.len() == 2 {
            later = Some(runs[1]);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let later = later.context("second run did not complete")?;
    let request = h.requests.lock().unwrap().last().unwrap().0.to_string();
    assert!(request.contains("[File /attachments/dot.png was deleted by the user.]"));
    assert!(!request.contains(DOT_PNG));
    let listing: Value =
        sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1 AND call_id='call_exec'")
            .bind(later)
            .fetch_one(&h.db)
            .await?;
    let listing = listing["stdout_head"].as_str().unwrap_or_default();
    assert!(
        !listing.contains("dot.png") && listing.contains("notes.csv"),
        "{listing}"
    );
    // A text file's content is never copied into stored context, so deleting it removes it
    // from every request after.
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM loop_steps WHERE run_id=$1 AND request_messages::text LIKE '%1,2%'",
    )
    .bind(run)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(stored, 0, "text attachment content was stored in context");
    let text_id = text["id"].as_str().unwrap();
    let removed = http
        .delete(format!("{}/orgs/{org}/attachments/{text_id}", h.base))
        .bearer_auth(&token)
        .send()
        .await?;
    assert!(removed.status().is_success());
    h.send(&user, conv, message("after the text is gone"))
        .await?;
    for _ in 0..400 {
        let done: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM runs WHERE conversation_id=$1 AND state='completed'",
        )
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
        if done == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let request = h.requests.lock().unwrap().last().unwrap().0.to_string();
    assert!(
        request.contains("[File /attachments/notes.csv was deleted by the user.]"),
        "{request}"
    );
    assert!(
        !request.contains("a,b\\n1,2"),
        "deleted text was sent again"
    );
    // Content goes with the last entry that holds it.
    let sha256: String = sqlx::query_scalar("SELECT sha256 FROM attachments WHERE id=$1")
        .bind(Uuid::parse_str(&image_id)?)
        .fetch_one(&h.db)
        .await?;
    let object = std::path::Path::new(&h.app.config.work.storage_root)
        .join(org.to_string())
        .join("attachments")
        .join(&sha256);
    ryzhub_server::attachments::gc::collect_org(&h.app, org).await?;
    let state: String = sqlx::query_scalar("SELECT state FROM attachments WHERE id=$1")
        .bind(Uuid::parse_str(&image_id)?)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(state, "reclaimed");
    assert!(object.exists(), "a live entry still holds the content");
    let copy_id = copy["id"].as_str().unwrap();
    http.delete(format!("{}/orgs/{org}/attachments/{copy_id}", h.base))
        .bearer_auth(&token)
        .send()
        .await?;
    ryzhub_server::attachments::gc::collect_org(&h.app, org).await?;
    assert!(!object.exists());
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS, secrets and scripts/start-execution-node.sh"]
async fn chat_tools_run_silently_and_viewed_images_reach_the_model() -> anyhow::Result<()> {
    use base64::Engine;
    let h = Harness::new().await?;
    ensure!(
        !h.app.config.sandbox.image.is_empty(),
        "execution image missing"
    );
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut owner = h.owner()?;
    h.send(&user, conv, message("viewimage")).await?;
    let run = h.wait_state(conv, "completed").await?;
    let actions: i64 = sqlx::query_scalar("SELECT count(*) FROM pending_actions WHERE run_id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(actions, 0, "sandbox tools must not ask");
    let result: Value =
        sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1 AND call_id='call_b'")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
    let sha256 = result["image"]["sha256"]
        .as_str()
        .with_context(|| format!("no image reference: {result}"))?
        .to_owned();
    assert!(
        !result.to_string().contains(DOT_PNG),
        "results keep a reference only"
    );
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT data FROM chat_images WHERE conversation_id=$1 AND sha256=$2")
            .bind(conv)
            .bind(&sha256)
            .fetch_one(&h.db)
            .await?;
    assert_eq!(
        base64::engine::general_purpose::STANDARD.encode(&stored),
        DOT_PNG
    );
    let expected =
        json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":DOT_PNG}});
    let sees_image = |body: &Value| {
        body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|message| {
                message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|block| block["type"] == "tool_result" && block["content"][0] == expected)
            })
    };
    ensure!(
        h.requests
            .lock()
            .unwrap()
            .last()
            .is_some_and(|(body, _)| sees_image(body)),
        "the tool result must carry the image"
    );
    // A later turn still sees it, while stored context holds only the reference.
    h.send(&user, conv, message("what was in it?")).await?;
    let mut completed = 0;
    for _ in 0..400 {
        completed = sqlx::query_scalar(
            "SELECT count(*) FROM runs WHERE conversation_id=$1 AND state='completed'",
        )
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
        if completed == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(completed, 2i64);
    ensure!(
        h.requests
            .lock()
            .unwrap()
            .last()
            .is_some_and(|(body, _)| sees_image(body)),
        "later turns must still see the image"
    );
    let copies: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM loop_steps s JOIN runs r ON r.id=s.run_id WHERE r.conversation_id=$1
         AND (coalesce(s.request_messages::text,'') LIKE '%'||$2||'%' OR coalesce(s.input_delta::text,'') LIKE '%'||$2||'%')",
    )
    .bind(conv)
    .bind(DOT_PNG)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(copies, 0, "model context must not store image bytes");
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS, secrets and scripts/start-execution-node.sh"]
async fn tool_completed_before_owner_crash_replays_once_after_host_restart() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    ensure!(
        !h.app.config.sandbox.image.is_empty(),
        "execution image missing"
    );
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut owner = h.owner()?;
    h.send(&user, conv, message("exec")).await?;
    // Sandbox tools run without asking.
    let run = h.wait_state(conv, "running").await?;
    let mut sandbox = None;
    for _ in 0..400 {
        sandbox = sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT sandbox_id FROM tool_calls WHERE run_id=$1 AND status='started'",
        )
        .bind(run)
        .fetch_optional(&h.db)
        .await?
        .flatten();
        if sandbox.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let sandbox = sandbox.context("tool did not start")?;
    // Allow dispatch to enter the guest, but terminate before the one-second command returns.
    tokio::time::sleep(Duration::from_millis(150)).await;
    owner.kill().await?;
    owner.wait().await?;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let status: String = sqlx::query_scalar("SELECT status FROM tool_calls WHERE run_id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(status, "started");
    let output = Process::new("docker")
        .args([
            "exec",
            "ryzhub-execution",
            "sh",
            "-c",
            "kill \"$(cat /state/host.pid)\"",
        ])
        .output()
        .await?;
    ensure!(output.status.success(), "host restart failed");
    let mut owner = h.owner()?;
    h.wait_state(conv, "completed").await?;
    let result: Value = sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(result["stdout_head"], "真实沙箱结果");
    let events: Vec<Value> =
        sqlx::query_scalar("SELECT payload FROM guest_events WHERE sandbox_id=$1 ORDER BY gseq")
            .bind(sandbox)
            .fetch_all(&h.db)
            .await?;
    assert_eq!(events.len(), 1, "{events:#?}");
    // On the quota volume when the node has one.
    let marker = format!(
        "org/{}/workspaces/{conv}/marker",
        user["org_id"].as_str().unwrap()
    );
    let read = format!("cat /state/volume/{marker} 2>/dev/null || cat /state/{marker}");
    let output = Process::new("docker")
        .args(["exec", "ryzhub-execution", "sh", "-c", &read])
        .output()
        .await?;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"once");
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and test gateway secrets"]
async fn virtual_key_lease_rotation_policy_rate_limit_and_interrupted_usage() -> anyhow::Result<()>
{
    use futures_util::StreamExt;
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    h.send(&user, conv, message("hello")).await?;
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
    sqlx::query("INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle) VALUES($1,$2,$3,$4,7,1,'running','{}')").bind(sandbox).bind(org).bind(conv).bind(run).execute(&h.db).await?;
    let request = |lease_epoch| keys::SandboxKeyRequest {
        org_id: org,
        user_id: id(&user, "user_id"),
        conversation_id: conv,
        run_id: run,
        sandbox_id: sandbox,
        lease_epoch,
        models: vec!["fixture-model".into()],
        credential_id: None,
    };
    let token = keys::issue_sandbox_key(&h.app.store, request(7))
        .await?
        .token;
    let client = reqwest::Client::new();
    let body = json!({"model":"fixture-model","max_tokens":32,"stream":true,"messages":[{"role":"user","content":"hello"}]});
    let send = |token: String, body: Value| {
        client
            .post(format!("{}/v1/messages", h.gateway))
            .bearer_auth(token)
            .json(&body)
            .send()
    };
    send(token.clone(), body.clone())
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    // Owner replacement changes exec_epoch; a live sandbox key remains valid on its lease.
    sqlx::query("UPDATE runs SET exec_epoch=2 WHERE id=$1")
        .bind(run)
        .execute(&h.db)
        .await?;
    let mut slow = body.clone();
    slow["messages"][0]["content"] = json!("slow key rotation");
    let mut active_stream = send(token.clone(), slow)
        .await?
        .error_for_status()?
        .bytes_stream();
    let mut received = active_stream
        .next()
        .await
        .context("missing first chunk")??
        .to_vec();
    let rotated = keys::issue_sandbox_key(&h.app.store, request(7))
        .await?
        .token;
    assert_eq!(send(token, body.clone()).await?.status(), 401);
    while let Some(chunk) = active_stream.next().await {
        received.extend_from_slice(&chunk?);
    }
    ensure!(
        String::from_utf8(received)?.contains("message_stop"),
        "key rotation interrupted an accepted stream"
    );
    let mut interrupted = body.clone();
    interrupted["messages"][0]["content"] = json!("interrupt");
    send(rotated.clone(), interrupted)
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let usage:(bool,String)=sqlx::query_as("SELECT estimated,status FROM usage_records WHERE run_id=$1 ORDER BY created_at DESC LIMIT 1").bind(run).fetch_one(&h.db).await?;
    assert_eq!(usage, (true, "interrupted".into()));
    let mut config = (*h.app.config).clone();
    config.llm.requests_per_minute_key = 4;
    let limited = App::new(h.app.store.clone(), h.app.nats.clone(), config)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1/messages", listener.local_addr()?);
    let task = tokio::spawn(async move {
        axum::serve(listener, gateway::router(limited))
            .await
            .unwrap();
    });
    // Rotated key has one accepted request so far; the limit is kept per key in the database.
    for _ in 0..3 {
        client
            .post(&url)
            .bearer_auth(&rotated)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
    }
    assert_eq!(
        client
            .post(&url)
            .bearer_auth(&rotated)
            .json(&body)
            .send()
            .await?
            .status(),
        429
    );
    task.abort();
    sqlx::query("UPDATE org_policies SET policy='{\"models\":[]}' WHERE org_id=$1")
        .bind(org)
        .execute(&h.db)
        .await?;
    assert_eq!(send(rotated.clone(), body.clone()).await?.status(), 403);
    sqlx::query("UPDATE org_policies SET policy='{}' WHERE org_id=$1")
        .bind(org)
        .execute(&h.db)
        .await?;
    sqlx::query("UPDATE sandboxes SET lease_epoch=8 WHERE id=$1")
        .bind(sandbox)
        .execute(&h.db)
        .await?;
    assert_eq!(send(rotated, body.clone()).await?.status(), 401);
    let expired = keys::issue_sandbox_key(&h.app.store, request(8))
        .await?
        .token;
    sqlx::query("UPDATE virtual_keys SET expires_at=now()-interval '1 second' WHERE token_hash=$1")
        .bind(hash_token(&expired))
        .execute(&h.db)
        .await?;
    assert_eq!(send(expired, body).await?.status(), 401);
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS, secrets and containerd"]
async fn guest_proxy_live_host_restart_and_terminal_cleanup_recovery() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut owner = h.owner()?;
    h.send(&user, conv, message("proxyexec")).await?;
    // Sandbox tools run without asking.
    let run = h.wait_state(conv, "running").await?;
    let mut sandbox = None;
    for _ in 0..400 {
        sandbox = sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT sandbox_id FROM tool_calls WHERE run_id=$1 AND status='started'",
        )
        .bind(run)
        .fetch_optional(&h.db)
        .await?
        .flatten();
        if sandbox.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let sandbox = sandbox.context("tool did not start")?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The key was delivered before the tool started; read the journal while the sandbox
    // still exists, since cleanup destroys its directory once the run ends.
    let journal = Process::new("docker")
        .args([
            "exec",
            "ryzhub-execution",
            "sh",
            "-c",
            // On the quota volume when the node has one.
            &format!(
                "cat /state/volume/sandboxes/{sandbox}/control/journal.json 2>/dev/null \
                 || cat /state/sandboxes/{sandbox}/control/journal.json"
            ),
        ])
        .output()
        .await?;
    ensure!(journal.status.success(), "guest journal missing");
    let journal = String::from_utf8(journal.stdout)?;
    assert!(!journal.contains("local-fixture-key") && !journal.contains("eyJhbGci"));
    let output = Process::new("docker")
        .args([
            "exec",
            "ryzhub-execution",
            "sh",
            "-c",
            "kill \"$(cat /state/host.pid)\"",
        ])
        .output()
        .await?;
    ensure!(output.status.success(), "host restart failed");
    h.wait_state(conv, "completed").await?;
    let result: Value = sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(result["exit_code"], 0, "{result}");
    assert!(
        result["stdout_head"]
            .as_str()
            .unwrap()
            .contains("event: ping")
    );
    let usages: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE run_id=$1 AND virtual_key_id IS NOT NULL AND status='completed' AND NOT estimated").bind(run).fetch_one(&h.db).await?;
    assert_eq!(usages, 1);
    owner.kill().await?;
    owner.wait().await?;
    // Simulate a stop whose acknowledgement/DB commit was lost after terminal publication.
    sqlx::query("UPDATE sandboxes SET state='running' WHERE id=$1")
        .bind(sandbox)
        .execute(&h.db)
        .await?;
    sqlx::query("UPDATE channels SET published_seq=last_seq WHERE conversation_id=$1")
        .bind(conv)
        .execute(&h.db)
        .await?;
    let mut owner = h.owner()?;
    let mut destroyed = false;
    for _ in 0..300 {
        destroyed = sqlx::query_scalar("SELECT state='deleted' FROM sandboxes WHERE id=$1")
            .bind(sandbox)
            .fetch_one(&h.db)
            .await?;
        if destroyed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        destroyed,
        "terminal-only conversation must be rediscovered for cleanup"
    );
    // Destroy unmounted the overlay and removed the sandbox directory.
    let gone = Process::new("docker")
        .args([
            "exec",
            "ryzhub-execution",
            "sh",
            "-c",
            &format!("! test -e /state/sandboxes/{sandbox} && ! test -e /state/volume/sandboxes/{sandbox} && ! grep -q /sandboxes/{sandbox}/ /proc/mounts"),
        ])
        .output()
        .await?;
    assert!(
        gone.status.success(),
        "destroyed sandbox left files or mounts"
    );
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and test gateway secrets"]
async fn explicit_credential_selection_revocation_and_tenant_boundary() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let outsider = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    let client = reqwest::Client::new();
    let token = user["token"].as_str().unwrap();
    let mut selected = Value::Null;
    for organization in [false, true] {
        let value: Value = client.post(format!("{}/orgs/{org}/credentials", h.base)).bearer_auth(token).json(&json!({"name":"test", "organization":organization,"api_key":if organization {"org-fixture-key"}else{"personal-fixture-key"}})).send().await?.error_for_status()?.json().await?;
        if organization {
            selected = value;
        }
    }
    let url = format!(
        "{}/orgs/{org}/conversations/{conv}/model-credential",
        h.base
    );
    assert_eq!(
        client
            .post(&url)
            .bearer_auth(outsider["token"].as_str().unwrap())
            .json(&json!({"credential_id":selected["id"]}))
            .send()
            .await?
            .status(),
        403
    );
    client
        .post(&url)
        .bearer_auth(token)
        .json(&json!({"credential_id":selected["id"]}))
        .send()
        .await?
        .error_for_status()?;
    h.send(&user, conv, message("hello")).await?;
    let lease = h
        .app
        .store
        .claim(org, conv, Uuid::now_v7(), 60_000)
        .await?
        .unwrap();
    owner::process_with_config(&h.app.store, &lease, &h.app.config).await?;
    let (run, credential): (Uuid, Option<Uuid>) =
        sqlx::query_as("SELECT id,credential_id FROM runs WHERE conversation_id=$1")
            .bind(conv)
            .fetch_one(&h.db)
            .await?;
    assert_eq!(credential, Some(id(&selected, "id")));
    let virtual_token = h.loop_key(&lease, id(&user, "user_id"), run).await?;
    let body = json!({"model":"fixture-model","stream":true,"max_tokens":32,"messages":[{"role":"user","content":"hello"}]});
    client
        .post(format!("{}/v1/messages", h.gateway))
        .bearer_auth(&virtual_token)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    assert_eq!(
        h.requests.lock().unwrap().last().unwrap().1["x-api-key"],
        "org-fixture-key"
    );
    let secrets = || {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM secrets WHERE org_id=$1")
            .bind(org)
            .fetch_one(&h.db)
    };
    assert_eq!(secrets().await?, 2);
    client
        .delete(format!(
            "{}/orgs/{org}/credentials/{}",
            h.base,
            selected["id"].as_str().unwrap()
        ))
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;
    // Revocation deletes the key outright.
    assert_eq!(secrets().await?, 1);
    assert_eq!(
        client
            .post(format!("{}/v1/messages", h.gateway))
            .bearer_auth(&virtual_token)
            .json(&body)
            .send()
            .await?
            .status(),
        503
    );
    assert_eq!(
        client
            .get(format!("{}/orgs/{org}/usage", h.base))
            .bearer_auth(outsider["token"].as_str().unwrap())
            .send()
            .await?
            .status(),
        403
    );
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and test gateway secrets"]
async fn legacy_credentials_move_to_secrets_and_key_rotation_rewraps_them() -> anyhow::Result<()> {
    use aes_gcm::{
        Aes256Gcm, KeyInit, Nonce,
        aead::{Aead, Payload},
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let h = Harness::new().await?;
    let user = h.user().await?;
    let (org, owner) = (id(&user, "org_id"), id(&user, "user_id"));
    // A personal credential in the original format: encrypted in place under vault key 0.
    let key = URL_SAFE_NO_PAD.decode(std::env::var("RYZHUB_TEST_VAULT_KEY")?)?;
    let credential = Uuid::now_v7();
    let nonce = [3u8; 12];
    let sealed = Aes256Gcm::new_from_slice(&key)?
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: b"legacy-fixture-key",
                aad: format!("{org}/{credential}/{owner}").as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("seal"))?;
    sqlx::query(
        "INSERT INTO model_credentials(id,org_id,owner_user_id,name,provider,encrypted_key,key_id)
         VALUES($1,$2,$3,'legacy','anthropic',$4,0)",
    )
    .bind(credential)
    .bind(org)
    .bind(owner)
    .bind([nonce.as_slice(), &sealed].concat())
    .execute(&h.db)
    .await?;
    let resolve = |app: App| async move {
        let mut tx = app.store.tx(org).await?;
        let policy = ryzhub_domain::Policy::default();
        let resolved = ryzhub_server::llm::credentials::resolve(
            &app,
            &mut tx,
            org,
            owner,
            None,
            &policy,
            "anthropic",
        )
        .await?;
        tx.commit().await?;
        anyhow::Ok(resolved)
    };
    let expected = (Some(credential), "legacy-fixture-key".to_owned());
    // Still readable before conversion.
    assert_eq!(resolve(h.app.clone()).await?, expected);
    let migrate = ryzhub_server::vault::migrate;
    assert_eq!(migrate(&h.db, &h.app.config.llm).await?, (1, 0));
    let (cipher, secret): (Option<Vec<u8>>, Option<Uuid>) =
        sqlx::query_as("SELECT encrypted_key,secret_id FROM model_credentials WHERE id=$1")
            .bind(credential)
            .fetch_one(&h.db)
            .await?;
    assert!(cipher.is_none() && secret.is_some());
    assert_eq!(resolve(h.app.clone()).await?, expected);
    assert_eq!(
        migrate(&h.db, &h.app.config.llm).await?,
        (0, 0),
        "idempotent"
    );

    // Rotate: key 1 becomes current and key 0 is retired. Rewrapping makes key 0 unnecessary.
    // SAFETY: integration tests run on one thread (--test-threads=1).
    unsafe {
        std::env::set_var(
            "RYZHUB_TEST_VAULT_KEY_NEXT",
            URL_SAFE_NO_PAD.encode([5u8; 32]),
        )
    };
    let mut rotated = (*h.app.config).clone();
    rotated.llm.vault_key_id = 1;
    rotated.llm.vault_key_env = "RYZHUB_TEST_VAULT_KEY_NEXT".into();
    rotated.llm.vault_retired_keys = [("0".into(), "RYZHUB_TEST_VAULT_KEY".into())].into();
    assert_eq!(migrate(&h.db, &rotated.llm).await?, (0, 1));
    let kek: i16 = sqlx::query_scalar("SELECT kek_id FROM secrets WHERE id=$1")
        .bind(secret)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(kek, 1);
    rotated.llm.vault_retired_keys.clear();
    let app = App::new(h.app.store.clone(), h.app.nats.clone(), rotated)?;
    assert_eq!(resolve(app).await?, expected);
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and an execution node"]
async fn sandboxes_record_and_route_to_their_execution_node() -> anyhow::Result<()> {
    use ryzhub_execution::HostCommand;
    use ryzhub_server::{config::NodeConfig, sandbox};
    let h = Harness::with(|config| {
        let primary = config.sandbox.host_socket.clone();
        config.sandbox.nodes = vec![
            NodeConfig {
                name: "primary".into(),
                host_socket: primary,
                ..Default::default()
            },
            NodeConfig {
                name: "spare".into(),
                host_socket: "/nonexistent/ryzhub-spare.sock".into(),
                ..Default::default()
            },
        ];
    })
    .await?;
    let user = h.user().await?;
    let (org, conv) = (id(&user, "org_id"), h.conversation(&user).await?);
    let mut owner = h.owner()?;
    h.send(&user, conv, message("exec")).await?;
    let run = h.wait_state(conv, "completed").await?;
    let result: Value = sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(result["exit_code"], 0, "{result}");
    let (sandbox_id, node): (Uuid, String) =
        sqlx::query_as("SELECT id,node FROM sandboxes WHERE run_id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
    assert_eq!(node, "primary", "placed on the first configured node");
    let registered: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_nodes WHERE name='primary')")
            .fetch_one(&h.db)
            .await?;
    assert!(registered);
    // The run's sandbox is destroyed once the run ends; a workspace purge waits for that.
    let mut destroyed = false;
    for _ in 0..400 {
        destroyed = sqlx::query_scalar("SELECT state='deleted' FROM sandboxes WHERE id=$1")
            .bind(sandbox_id)
            .fetch_one(&h.db)
            .await?;
        if destroyed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(destroyed);
    // Workspace-wide commands reach only nodes the workspace used: the unreachable spare
    // does not block them.
    let purge = HostCommand::PurgeWorkspace {
        org_id: org,
        workspace_id: conv,
        artifacts: vec![],
    };
    sandbox::host_workspace(&h.app, org, conv, purge).await?;
    // RPCs about a sandbox follow the node its row records.
    sqlx::query("INSERT INTO execution_nodes(name) VALUES('spare')")
        .execute(&h.db)
        .await?;
    sqlx::query("UPDATE sandboxes SET node='spare' WHERE id=$1")
        .bind(sandbox_id)
        .execute(&h.db)
        .await?;
    assert_eq!(sandbox::node_of(&h.app, org, sandbox_id).await?, "spare");
    let destroy = HostCommand::Destroy {
        sandbox_id,
        exec_epoch: 1,
    };
    assert!(
        sandbox::host(&h.app, org, sandbox_id, destroy)
            .await
            .is_err()
    );
    // Placement: the node the conversation last used, else the least loaded open node; never
    // a node known to be down or draining.
    let place = |conversation: Uuid| {
        let app = h.app.clone();
        async move {
            let mut tx = app.store.tx(org).await?;
            let node = sandbox::place(&mut tx, &app, conversation).await?;
            tx.commit().await?;
            anyhow::Ok(node)
        }
    };
    sqlx::query("UPDATE sandboxes SET state='running' WHERE id=$1")
        .bind(sandbox_id)
        .execute(&h.db)
        .await?;
    // The conversation's last sandbox is on `spare` (and running there).
    assert_eq!(place(conv).await?, "spare", "affinity");
    let fresh = h.conversation(&user).await?;
    assert_eq!(place(fresh).await?, "primary", "least loaded");
    sqlx::query(
        "UPDATE execution_nodes SET lease_until=now()-interval '1 minute' WHERE name='spare'",
    )
    .execute(&h.db)
    .await?;
    assert_eq!(
        place(conv).await?,
        "primary",
        "a node known to be down is skipped"
    );
    sqlx::query("UPDATE execution_nodes SET lease_until=NULL,state='draining' WHERE name='spare'")
        .execute(&h.db)
        .await?;
    assert_eq!(
        place(conv).await?,
        "primary",
        "a draining node takes no sandboxes"
    );
    sqlx::query("UPDATE sandboxes SET state='deleted' WHERE id=$1")
        .bind(sandbox_id)
        .execute(&h.db)
        .await?;
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS, an execution node and internet access"]
async fn sandboxes_reach_allowlisted_hosts_only_and_egress_is_audited() -> anyhow::Result<()> {
    use ryzhub_server::sandbox;
    let h = Harness::new().await?;
    let user = h.user().await?;
    let (org, conv) = (id(&user, "org_id"), h.conversation(&user).await?);
    let mut owner = h.owner()?;
    h.send(&user, conv, message("egresscheck")).await?;
    let run = h.wait_state(conv, "completed").await?;
    let result: Value = sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    let stdout = result["stdout_head"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("APK_OK"),
        "allowlisted mirror unreachable: {result}"
    );
    assert!(
        !stdout.contains("WGET=0"),
        "unlisted host reached: {result}"
    );
    assert!(
        stdout.contains("DIRECT=") && !stdout.contains("DIRECT=0"),
        "bypassed the proxy: {result}"
    );
    assert!(stdout.contains("http://127.0.0.1:8091"), "{result}");
    assert!(sandbox::collect_egress(&h.app, "default").await? > 0);
    let audits: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT resource,result,detail FROM audit_logs WHERE org_id=$1 AND action='egress.connect'",
    )
    .bind(org)
    .fetch_all(&h.db)
    .await?;
    assert!(
        audits.iter().any(|(resource, result, detail)| resource
            .starts_with("dl-cdn.alpinelinux.org:")
            && result == "allowed"
            && detail["received"].as_u64() > Some(0)),
        "{audits:?}"
    );
    assert!(
        audits
            .iter()
            .any(|(resource, result, detail)| resource == "example.com:80"
                && result == "denied"
                && detail["reason"] == "host_not_allowed"),
        "{audits:?}"
    );
    // Collected records are stored once.
    let stored = audits.len();
    sandbox::collect_egress(&h.app, "default").await?;
    let again: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs WHERE org_id=$1 AND action='egress.connect'",
    )
    .bind(org)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(again as usize, stored);
    // Organization policy narrows the instance allowlist; an empty list means no network.
    sqlx::query(
        "INSERT INTO org_policies(org_id,version,policy)
         SELECT org_id,version+1,policy||'{\"egress_hosts\":[\"pypi.org\",\"unlisted.example\"]}'
         FROM org_policies WHERE org_id=$1 ORDER BY version DESC LIMIT 1",
    )
    .bind(org)
    .execute(&h.db)
    .await?;
    let mut tx = h.app.store.tx(org).await?;
    assert_eq!(
        sandbox::limits(&mut tx, &h.app, org).await?.egress,
        ["pypi.org"]
    );
    tx.commit().await?;
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and an execution node with a quota volume"]
async fn a_full_sandbox_gets_enospc_and_the_node_keeps_working() -> anyhow::Result<()> {
    let h = Harness::with(|config| {
        config.sandbox.disk_bytes = 256 * 1024 * 1024;
        config.sandbox.memory_bytes = 1024 * 1024 * 1024;
    })
    .await?;
    let user = h.user().await?;
    let (org, conv) = (id(&user, "org_id"), h.conversation(&user).await?);
    let mut owner = h.owner()?;
    h.send(&user, conv, message("quotacheck")).await?;
    let run = h.wait_state(conv, "completed").await?;
    let result: Value = sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    let stdout = result["stdout_head"].as_str().unwrap_or_default();
    let full = stdout.matches("No space left on device").count()
        + stdout.matches("Disk quota exceeded").count();
    assert_eq!(
        full, 2,
        "workspace and root layer must both be limited: {result}"
    );
    // Out of memory, the kernel kills the command and the guest keeps running the sandbox.
    assert!(stdout.contains("HOG=137"), "{result}");
    assert!(stdout.contains("SMALL_OK"), "{result}");
    // The workspace lives on the quota volume.
    let placed = Process::new("docker")
        .args([
            "exec",
            "ryzhub-execution",
            "test",
            "-f",
            &format!("/state/volume/org/{org}/workspaces/{conv}/ok"),
        ])
        .status()
        .await?;
    assert!(placed.success());
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and an execution node"]
async fn chat_sandboxes_are_reused_while_idle_and_workspaces_survive_node_loss()
-> anyhow::Result<()> {
    let h = Harness::with(|config| config.sandbox.chat_idle_secs = 600).await?;
    let user = h.user().await?;
    let (org, conv) = (id(&user, "org_id"), h.conversation(&user).await?);
    let mut owner = h.owner()?;
    let db = &h.db;
    let marker = |run: Uuid| async move {
        let result: Value = sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1")
            .bind(run)
            .fetch_one(db)
            .await?;
        anyhow::Ok(
            result["stdout_head"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
    };
    let checkpoints = || {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM checkpoints WHERE conversation_id=$1 AND state='committed'
             AND manifest->>'scope'='workspace'",
        )
        .bind(conv)
        .fetch_one(&h.db)
    };
    h.send(&user, conv, message("markercat")).await?;
    let first = h.wait_state(conv, "completed").await?;
    assert_eq!(marker(first).await?, "once");
    // The run's end checkpoints the workspace and keeps the sandbox for the next run.
    let mut kept = false;
    for _ in 0..400 {
        kept = checkpoints().await? == 1
            && sqlx::query_scalar(
                "SELECT idle_until>now() FROM sandboxes WHERE conversation_id=$1 AND state='running'",
            )
            .bind(conv)
            .fetch_optional(&h.db)
            .await?
            .unwrap_or(false);
        if kept {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(kept, "workspace checkpointed and sandbox kept idle");
    h.send(&user, conv, message("markercat")).await?;
    let second = h.wait_after(conv, first).await?;
    assert_eq!(marker(second).await?, "onceonce");
    let sandboxes: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM sandboxes WHERE conversation_id=$1")
            .bind(conv)
            .fetch_all(&h.db)
            .await?;
    assert_eq!(sandboxes.len(), 1, "the idle sandbox served the second run");
    // Expire it, then lose the node's copy of the workspace.
    let mut gone = false;
    for _ in 0..400 {
        sqlx::query("UPDATE sandboxes SET idle_until=now()-interval '1 second' WHERE id=$1 AND idle_until IS NOT NULL")
            .bind(sandboxes[0])
            .execute(&h.db)
            .await?;
        gone = sqlx::query_scalar("SELECT state='deleted' FROM sandboxes WHERE id=$1")
            .bind(sandboxes[0])
            .fetch_one(&h.db)
            .await?;
        if gone {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "expired sandbox destroyed");
    assert_eq!(
        checkpoints().await?,
        2,
        "the second run's changes were checkpointed"
    );
    let lost = Process::new("docker")
        .args([
            "exec",
            "ryzhub-execution",
            "sh",
            "-c",
            &format!(
                "rm -rf /state/org/{org}/workspaces/{conv} /state/volume/org/{org}/workspaces/{conv}"
            ),
        ])
        .status()
        .await?;
    assert!(lost.success());
    h.send(&user, conv, message("markercat")).await?;
    let third = h.wait_after(conv, second).await?;
    assert_eq!(
        marker(third).await?,
        "onceonceonce",
        "restored from the checkpoint"
    );
    // A stopped sandbox that later owners fenced with newer epochs than its row records is
    // still reclaimed.
    let mut idle = None;
    for _ in 0..400 {
        idle = sqlx::query_scalar(
            "SELECT id FROM sandboxes WHERE conversation_id=$1 AND state='running' AND idle_until IS NOT NULL",
        )
        .bind(conv)
        .fetch_optional(&h.db)
        .await?;
        if idle.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let idle: Uuid = idle.context("the third run's sandbox is kept idle")?;
    owner.kill().await?;
    owner.wait().await?;
    let epoch: i64 = sqlx::query_scalar(
        "UPDATE channels SET writer_epoch=writer_epoch+40 WHERE conversation_id=$1 RETURNING writer_epoch",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    let fence = ryzhub_execution::HostCommand::Stop {
        sandbox_id: idle,
        exec_epoch: epoch as u64,
    };
    ryzhub_server::sandbox::host(&h.app, org, idle, fence).await?;
    sqlx::query(
        "UPDATE sandboxes SET persistent=true,state='lost',node_lease_until=now()-interval '2 days' WHERE id=$1",
    )
    .bind(idle)
    .execute(&h.db)
    .await?;
    let worker = PgPool::connect(&h.worker_url).await?;
    ryzhub_server::sandbox::reclaim_once(&h.app, &worker).await?;
    worker.close().await;
    let state: String = sqlx::query_scalar("SELECT state FROM sandboxes WHERE id=$1")
        .bind(idle)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(state, "deleted", "reclaimed despite the newer fence");
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and an execution node"]
async fn nodes_shed_unused_workspaces_and_stopped_sandboxes_and_report_stale_checkpoints()
-> anyhow::Result<()> {
    let h = Harness::with(|config| config.sandbox.workspace_evict_after_secs = 1).await?;
    let worker = PgPool::connect(&h.worker_url).await?;
    let user = h.user().await?;
    let (org, conv) = (id(&user, "org_id"), h.conversation(&user).await?);
    let mut owner = h.owner()?;
    h.send(&user, conv, message("markercat")).await?;
    let first = h.wait_state(conv, "completed").await?;
    let mut settled = false;
    for _ in 0..400 {
        settled = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspace_heads h JOIN checkpoints c ON c.id=h.checkpoint_id
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
    tokio::time::sleep(Duration::from_millis(1500)).await;
    ryzhub_server::sandbox::reclaim_once(&h.app, &worker).await?;
    let evicted: bool = sqlx::query_scalar(
        "SELECT evicted_at IS NOT NULL FROM workspace_heads WHERE conversation_id=$1",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    assert!(evicted);
    let gone = Process::new("docker")
        .args([
            "exec",
            "ryzhub-execution",
            "sh",
            "-c",
            &format!("! test -e /state/volume/org/{org}/workspaces/{conv} && ! test -e /state/org/{org}/workspaces/{conv}"),
        ])
        .status()
        .await?;
    assert!(gone.success(), "the node no longer holds the workspace");
    h.send(&user, conv, message("markercat")).await?;
    let second = h.wait_after(conv, first).await?;
    let result: Value = sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1")
        .bind(second)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(result["stdout_head"], "onceonce", "restored on demand");

    // A suspended Work sandbox, or a Chat one an earlier version only stopped, is destroyed
    // once its grace period is over; a running one whose checkpoints fall behind is reported
    // once.
    let other = h.conversation(&user).await?;
    h.send(&user, other, message("hello")).await?;
    let run = h.wait_state(other, "completed").await?;
    let (suspended, stopped, running) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for (sandbox, state, persistent, age) in [
        (suspended, "suspended", true, "2 days"),
        (stopped, "stopped", false, "2 days"),
        (running, "running", true, "1 hour"),
    ] {
        sqlx::query(
            "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent,node_lease_until,created_at)
             VALUES($1,$2,$3,$4,1,1,$5,'{}',$6,now()-$7::interval,now()-$7::interval)",
        )
        .bind(sandbox)
        .bind(org)
        .bind(other)
        .bind(run)
        .bind(state)
        .bind(persistent)
        .bind(age)
        .execute(&h.db)
        .await?;
    }
    for _ in 0..2 {
        ryzhub_server::sandbox::reclaim_once(&h.app, &worker).await?;
    }
    for sandbox in [suspended, stopped] {
        let state: String = sqlx::query_scalar("SELECT state FROM sandboxes WHERE id=$1")
            .bind(sandbox)
            .fetch_one(&h.db)
            .await?;
        assert_eq!(state, "deleted");
    }
    let reports: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs WHERE org_id=$1 AND action='workspace.checkpoint_stale' AND resource=$2",
    )
    .bind(org)
    .bind(other.to_string())
    .fetch_one(&h.db)
    .await?;
    assert_eq!(reports, 1, "reported once an hour");
    let status: Value = reqwest::Client::new()
        .get(format!(
            "{}/orgs/{org}/conversations/{other}/workspace",
            h.base
        ))
        .bearer_auth(user["token"].as_str().unwrap())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(status["stale"], true);
    sqlx::query("UPDATE sandboxes SET state='deleted' WHERE id=$1")
        .bind(running)
        .execute(&h.db)
        .await?;
    worker.close().await;
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and an execution node"]
async fn files_move_between_attachments_the_workspace_and_the_library() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let (org, conv) = (id(&user, "org_id"), h.conversation(&user).await?);
    let notes = h.upload(&user, "notes.csv", b"a,b\n1,2\n".to_vec()).await?;
    let mut owner = h.owner()?;
    h.send(
        &user,
        conv,
        Command::SendMessage(SendMessage {
            text: "fscheck".into(),
            attachment_ids: vec![notes["id"].as_str().unwrap().into()],
            ..Default::default()
        }),
    )
    .await?;
    let run = h.wait_state(conv, "completed").await?;
    let results: Vec<(String, Value)> =
        sqlx::query_as("SELECT call_id,result FROM tool_calls WHERE run_id=$1 ORDER BY call_id")
            .bind(run)
            .fetch_all(&h.db)
            .await?;
    let result = |call: &str| results.iter().find(|(c, _)| c == call).unwrap().1.clone();
    assert_eq!(result("call_a")["exit_code"], 0, "{:?}", result("call_a"));
    assert_eq!(result("call_b")["stdout_head"], "a,b\n1,2\n");
    let handed = result("call_c");
    assert_eq!(handed["name"], "out.txt", "{handed}");
    assert_eq!(handed["path"], "/attachments/out.txt");
    // The file is in the user's library and this conversation.
    let library: Value = reqwest::Client::new()
        .get(format!("{}/orgs/{org}/attachments?q=out.txt", h.base))
        .bearer_auth(user["token"].as_str().unwrap())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(library.to_string().contains("out.txt"), "{library}");
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS, Docker and an execution node"]
async fn profiles_layer_dependencies_and_unused_images_leave_the_node() -> anyhow::Result<()> {
    use ryzhub_server::config::ProfileConfig;
    // A dependency layer holding one marker file.
    let tree = tempfile::tempdir()?;
    let marker = format!("deps-{}", Uuid::now_v7().simple());
    std::fs::create_dir_all(tree.path().join("opt/ryzhub-deps"))?;
    std::fs::write(tree.path().join("opt/ryzhub-deps/marker"), &marker)?;
    let built = Process::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/build-deps.sh"
    ))
    .arg(tree.path())
    .output()
    .await?;
    ensure!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let deps = String::from_utf8(built.stdout)?.trim().to_owned();
    let h = Harness::with(|config| {
        config.sandbox.profiles = vec![ProfileConfig {
            name: "with-deps".into(),
            deps: deps.clone(),
            ..Default::default()
        }];
    })
    .await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let created: Value = reqwest::Client::new()
        .post(format!("{}/orgs/{org}/conversations", h.base))
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"title": "deps", "mode": "chat", "run_config": {"env_profile": "with-deps"}}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let conv = id(&created, "id");
    let mut owner = h.owner()?;
    h.send(&user, conv, message("depscheck")).await?;
    let run = h.wait_state(conv, "completed").await?;
    let result: Value = sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(result["stdout_head"], marker.as_str(), "{result}");
    let recorded: Option<String> =
        sqlx::query_scalar("SELECT handle->>'deps' FROM sandboxes WHERE run_id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
    assert_eq!(recorded.as_deref(), Some(deps.as_str()));
    let held = || async {
        let listing = Process::new("docker")
            .args([
                "exec",
                "ryzhub-execution",
                "ctr",
                "--address",
                "/run/ryzhub-containerd.sock",
                "-n",
                "ryzhub",
                "images",
                "list",
                "-q",
            ])
            .output()
            .await?;
        anyhow::Ok(
            String::from_utf8(listing.stdout)?
                .lines()
                .any(|l| l.trim() == deps),
        )
    };
    // Configured and used: kept.
    let worker = PgPool::connect(&h.worker_url).await?;
    let mut keep = (*h.app.config).clone();
    keep.sandbox.image_gc_after_secs = 0;
    let app = App::new(h.app.store.clone(), h.app.nats.clone(), keep.clone())?;
    let mut gone = false;
    for _ in 0..400 {
        gone = sqlx::query_scalar("SELECT state='deleted' FROM sandboxes WHERE run_id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
        if gone {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(gone, "sandbox destroyed");
    for _ in 0..2 {
        ryzhub_server::sandbox::collect_images(&app, &worker).await?;
    }
    assert!(held().await?, "a configured layer stays");
    // Once no profile names it and nothing uses it, it leaves the node.
    keep.sandbox.profiles.clear();
    let app = App::new(h.app.store.clone(), h.app.nats.clone(), keep)?;
    ryzhub_server::sandbox::collect_images(&app, &worker).await?;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    ryzhub_server::sandbox::collect_images(&app, &worker).await?;
    assert!(!held().await?, "the unused layer was removed");
    worker.close().await;
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS"]
async fn side_query_reads_active_committed_prefix_without_dangling_tool_calls() -> anyhow::Result<()>
{
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut owner = h.owner()?;
    h.send(&user, conv, message("question latest committed prefix"))
        .await?;
    h.wait_state(conv, "requires_action").await?;
    let cid = h
        .send(
            &user,
            conv,
            Command::SideQuery(SideQuery {
                text: "read current context".into(),
            }),
        )
        .await?;
    let mut seed = Value::Null;
    for _ in 0..200 {
        let saved: Option<Value> =
            sqlx::query_scalar("SELECT seed_messages FROM runs WHERE cause_cid=$1")
                .bind(cid)
                .fetch_optional(&h.db)
                .await?;
        if let Some(v) = saved {
            seed = v;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(seed.to_string().contains("latest committed prefix"));
    assert!(!seed.to_string().contains("tool_use"));
    assert!(seed.to_string().contains("signed-block"));
    owner.kill().await?;
    h.close().await
}
#[tokio::test]
#[ignore = "requires PostgreSQL/NATS"]
async fn removing_member_revokes_an_inflight_model_stream() -> anyhow::Result<()> {
    use futures_util::StreamExt;
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    let uid = id(&user, "user_id");
    h.send(&user, conv, message("unused")).await?;
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
    let token = h.loop_key(&lease, uid, run).await?;
    let response=reqwest::Client::new().post(format!("{}/v1/messages",h.gateway)).bearer_auth(token).json(&json!({"model":"fixture-model","max_tokens":32,"stream":true,"messages":[{"role":"user","content":"slow"}]})).send().await?.error_for_status()?;
    let mut stream = response.bytes_stream();
    assert!(stream.next().await.context("no stream")?.is_ok());
    sqlx::query("DELETE FROM memberships WHERE org_id=$1 AND user_id=$2")
        .bind(org)
        .bind(uid)
        .execute(&h.db)
        .await?;
    let mut complete = false;
    while let Some(chunk) = stream.next().await {
        if let Ok(chunk) = chunk {
            complete |= String::from_utf8_lossy(&chunk).contains("message_stop");
        }
    }
    assert!(!complete);
    // The record is finalized right after the stream is cut.
    let mut usage = (String::new(), false);
    for _ in 0..100 {
        usage = sqlx::query_as("SELECT status,estimated FROM usage_records WHERE run_id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
        if usage.0 != "started" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(usage, ("interrupted".into(), true));
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and an execution node"]
async fn chat_commands_stream_their_output_as_they_write_it() -> anyhow::Result<()> {
    use futures_util::StreamExt;
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut frames = h
        .app
        .nats
        .subscribe(owner::subject(&format!("conv:{conv}")))
        .await?;
    let mut owner = h.owner()?;
    h.send(&user, conv, message("streamcheck")).await?;
    // The output streams in pieces while the command runs, beside the call's own text.
    let (mut pieces, mut output) = (0, String::new());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    while !output.contains("line3") {
        let message = tokio::time::timeout_at(deadline, frames.next())
            .await?
            .context("stream ended")?;
        let frame = Frame::decode(message.payload)?;
        if let Some(frame::Body::Delta(delta)) = frame.body
            && delta.stream == "terminal_output"
        {
            assert_eq!(
                delta.offset as usize,
                output.len(),
                "pieces follow one another"
            );
            output.push_str(&delta.text);
            pieces += 1;
        }
    }
    assert!(pieces >= 2, "{pieces} piece(s): {output:?}");
    assert_eq!(output, "line1\nline2\nline3\n");
    let run = h.wait_state(conv, "completed").await?;
    let result: Value = sqlx::query_scalar("SELECT result FROM tool_calls WHERE run_id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(result["stdout_head"], "line1\nline2\nline3\n");
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

/// Chat Completions as an OpenAI-compatible upstream answers: usage in a last chunk of its own
/// when the stream asked for it.
async fn chat_completions_upstream(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    requests.lock().unwrap().push((body.clone(), headers));
    if body["stream"] != true {
        return Json(json!({"id":"c2","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"你好"},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":2}})).into_response();
    }
    let chunks = [
        json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"你好"}}]}),
        json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"id":"c1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}),
    ];
    let mut text: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    text.push_str("data: [DONE]\n\n");
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(text))
        .unwrap()
}

/// Gemini: every chunk carries the usage so far; thoughts are counted apart.
async fn gemini_upstream(
    State(requests): State<Requests>,
    axum::extract::Path(target): axum::extract::Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    mut headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    headers.insert("x-target", target.parse().unwrap());
    headers.insert("x-query", query.unwrap_or_default().parse().unwrap());
    requests.lock().unwrap().push((body, headers));
    if !target.ends_with(":streamGenerateContent") {
        return Json(json!({"candidates":[{"content":{"parts":[{"text":"你好"}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":4,"thoughtsTokenCount":6}})).into_response();
    }
    let chunks = [
        json!({"candidates":[{"content":{"parts":[{"text":"你"}],"role":"model"}}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":1}}),
        json!({"candidates":[{"content":{"parts":[{"text":"好"}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":2,"thoughtsTokenCount":5}}),
    ];
    let text: String = chunks
        .iter()
        .map(|c| format!("data: {c}\r\n\r\n"))
        .collect();
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(text))
        .unwrap()
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and test gateway secrets"]
async fn gateway_speaks_chat_completions_and_gemini_fails_over_caps_reasoning_and_retains_content()
-> anyhow::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fixtures = Requests::default();
    let good = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let down = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let good_url = format!("http://{}", good.local_addr()?);
    let down_url = format!("http://{}", down.local_addr()?);
    // A port nothing listens on.
    let unreachable_url = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        format!("http://{}", listener.local_addr()?)
    };
    let refused = Arc::new(AtomicUsize::new(0));
    let state = fixtures.clone();
    let fixture_task = tokio::spawn(async move {
        let app = Router::new()
            .route("/v1/chat/completions", post(chat_completions_upstream))
            .route("/v1beta/models/{target}", post(gemini_upstream))
            .with_state(state);
        axum::serve(good, app).await.unwrap();
    });
    let counter = refused.clone();
    let down_task = tokio::spawn(async move {
        let app = Router::new().fallback(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { axum::http::StatusCode::SERVICE_UNAVAILABLE }
        });
        axum::serve(down, app).await.unwrap();
    });
    let h = Harness::with(|c| {
        c.llm.chat_completions_url = good_url.clone();
        c.llm.chat_completions_key_env = "RYZHUB_TEST_UPSTREAM_KEY".into();
        c.llm.gemini_url = good_url.clone();
        c.llm.gemini_key_env = "RYZHUB_TEST_UPSTREAM_KEY".into();
        let upstream = |url: &str, priority, model: &str| Upstream {
            url: url.into(),
            priority,
            model: model.into(),
            ..Default::default()
        };
        c.llm.models.extend([
            ModelRoute {
                id: "gpt-fixture".into(),
                protocol: "chat_completions".into(),
                harnesses: vec![],
                upstreams: vec![
                    upstream(&down_url, 0, ""),
                    upstream(&unreachable_url, 1, ""),
                    upstream(&good_url, 2, "gpt-upstream"),
                ],
                ..Default::default()
            },
            ModelRoute {
                id: "gemini-fixture".into(),
                protocol: "gemini".into(),
                harnesses: vec![],
                ..Default::default()
            },
            ModelRoute {
                id: "down-fixture".into(),
                protocol: "chat_completions".into(),
                harnesses: vec![],
                upstreams: vec![upstream(&down_url, 0, "")],
                ..Default::default()
            },
        ]);
    })
    .await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    let uid = id(&user, "user_id");
    h.send(&user, conv, message("hello")).await?;
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
    let models = [
        "fixture-model",
        "gpt-fixture",
        "gemini-fixture",
        "down-fixture",
    ];
    let token = keys::issue_loop_key(
        &h.app.store,
        &lease,
        uid,
        run,
        models.iter().map(|m| m.to_string()).collect(),
    )
    .await?;
    let client = reqwest::Client::new();
    let post = |path: &str, body: Value| {
        client
            .post(format!("{}{path}", h.gateway))
            .bearer_auth(&token)
            .json(&body)
            .send()
    };
    let last_usage = || async {
        anyhow::Ok(
            sqlx::query(
                "SELECT id,input_tokens,output_tokens,estimated,status,provider FROM usage_records
                 WHERE run_id=$1 ORDER BY id DESC LIMIT 1",
            )
            .bind(run)
            .fetch_one(&h.db)
            .await?,
        )
    };
    let tokens = |row: &sqlx::postgres::PgRow| {
        (
            row.get::<i64, _>("input_tokens"),
            row.get::<i64, _>("output_tokens"),
            row.get::<bool, _>("estimated"),
            row.get::<String, _>("status"),
        )
    };

    // Chat Completions, streamed: past a refusing and an unreachable upstream, under the
    // upstream's own model id, with usage asked for.
    let chat =
        json!({"model":"gpt-fixture","stream":true,"messages":[{"role":"user","content":"hi"}]});
    let text = post("/v1/chat/completions", chat.clone())
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert!(text.contains("你好") && text.contains("[DONE]"), "{text}");
    assert_eq!(refused.load(Ordering::SeqCst), 1);
    let (received, headers) = fixtures.lock().unwrap().last().unwrap().clone();
    assert_eq!(received["model"], "gpt-upstream");
    assert_eq!(received["stream_options"]["include_usage"], true);
    assert_eq!(headers["authorization"], "Bearer local-fixture-key");
    let usage = last_usage().await?;
    assert_eq!(tokens(&usage), (11, 3, false, "completed".into()));
    assert_eq!(usage.get::<String, _>("provider"), "openai");
    let mut whole = chat.clone();
    whole["stream"] = json!(false);
    post("/v1/chat/completions", whole)
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    assert_eq!(
        tokens(&last_usage().await?),
        (9, 2, false, "completed".into())
    );

    // Gemini, streamed and whole; thoughts count as output.
    let gemini = json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]});
    let text = post(
        "/v1beta/models/gemini-fixture:streamGenerateContent?alt=sse",
        gemini.clone(),
    )
    .await?
    .error_for_status()?
    .text()
    .await?;
    assert!(text.contains("STOP"), "{text}");
    let (_, headers) = fixtures.lock().unwrap().last().unwrap().clone();
    assert_eq!(headers["x-target"], "gemini-fixture:streamGenerateContent");
    assert_eq!(headers["x-query"], "alt=sse");
    assert_eq!(headers["x-goog-api-key"], "local-fixture-key");
    assert!(headers.get("authorization").is_none());
    let usage = last_usage().await?;
    assert_eq!(tokens(&usage), (20, 7, false, "completed".into()));
    assert_eq!(usage.get::<String, _>("provider"), "gemini");
    post(
        "/v1beta/models/gemini-fixture:generateContent",
        gemini.clone(),
    )
    .await?
    .error_for_status()?
    .bytes()
    .await?;
    assert_eq!(
        tokens(&last_usage().await?),
        (20, 10, false, "completed".into())
    );

    // The last upstream's refusal is the answer.
    let mut refusal = chat.clone();
    refusal["model"] = json!("down-fixture");
    let refused_response = post("/v1/chat/completions", refusal).await?;
    assert_eq!(refused_response.status(), 503);
    // Accounting ends before the body does.
    refused_response.bytes().await?;
    assert_eq!(tokens(&last_usage().await?).3, "upstream_error");

    // Token counting is answered without a usage record.
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE run_id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    let counted: Value = post(
        "/v1/messages/count_tokens",
        json!({"model":"fixture-model","messages":[{"role":"user","content":"hi"}]}),
    )
    .await?
    .error_for_status()?
    .json()
    .await?;
    assert_eq!(counted["input_tokens"], 7);
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE run_id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(before, after);

    // The models a key may use, in either format.
    let listed: Value = client
        .get(format!("{}/v1/models", h.gateway))
        .bearer_auth(&token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(ids, models);
    let anthropic: Value = client
        .get(format!("{}/v1/models", h.gateway))
        .bearer_auth(&token)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(anthropic["data"][0]["type"], "model");

    // A reasoning cap refuses requests above it, in each protocol's terms; retention keeps
    // what passes.
    sqlx::query(
        "UPDATE org_policies SET policy='{\"max_reasoning\":\"low\",\"retain_model_content_days\":3}'
         WHERE org_id=$1",
    )
    .bind(org)
    .execute(&h.db)
    .await?;
    let mut high = chat.clone();
    high["reasoning_effort"] = json!("high");
    let denied = post("/v1/chat/completions", high).await?;
    assert_eq!(denied.status(), 403);
    assert!(denied.text().await?.contains("REASONING_DENIED"));
    let mut thinking = gemini.clone();
    thinking["generationConfig"]["thinkingConfig"]["thinkingBudget"] = json!(20000);
    assert_eq!(
        post("/v1beta/models/gemini-fixture:generateContent", thinking)
            .await?
            .status(),
        403
    );
    let mut deep = json!({"model":"fixture-model","max_tokens":32,"stream":true,"messages":[{"role":"user","content":"hello"}]});
    deep["thinking"] = json!({"type":"enabled","budget_tokens":30000});
    assert_eq!(post("/v1/messages", deep).await?.status(), 403);
    let mut low = chat.clone();
    low["reasoning_effort"] = json!("low");
    post("/v1/chat/completions", low)
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let kept: Uuid = last_usage().await?.get("id");
    let expires: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT content_expires_at FROM usage_records WHERE id=$1")
            .bind(kept)
            .fetch_one(&h.db)
            .await?;
    let days = (expires.context("content kept")? - chrono::Utc::now()).num_hours();
    assert!((71..=72).contains(&days), "{days}");
    let content_url = format!("{}/orgs/{org}/usage/{kept}/content", h.base);
    let content: Value = client
        .get(&content_url)
        .bearer_auth(user["token"].as_str().unwrap())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        content["request"]
            .as_str()
            .unwrap()
            .contains("\"reasoning_effort\":\"low\"")
    );
    assert!(content["response"].as_str().unwrap().contains("你好"));

    // Expired content goes.
    sqlx::query(
        "UPDATE usage_records SET content_expires_at=now()-interval '1 second' WHERE id=$1",
    )
    .bind(kept)
    .execute(&h.db)
    .await?;
    let worker = PgPool::connect(&h.worker_url).await?;
    gateway::purge_model_content(&h.app, &worker).await?;
    worker.close().await;
    let gone = client
        .get(&content_url)
        .bearer_auth(user["token"].as_str().unwrap())
        .send()
        .await?;
    assert_eq!(gone.status(), 404);
    let blobs = std::path::Path::new(&h.app.config.work.storage_root)
        .join(org.to_string())
        .join("llm")
        .join(kept.to_string());
    assert_eq!(std::fs::read_dir(&blobs)?.count(), 0);

    fixture_task.abort();
    down_task.abort();
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS, secrets and containerd"]
async fn chat_contexts_move_old_results_out_and_compact() -> anyhow::Result<()> {
    let h = Harness::with(|c| {
        c.llm.context_budget_tokens = 2000;
        c.llm.compaction_models = vec!["fixture-model".into()];
    })
    .await?;
    let user = h.user().await?;
    let mut owner = h.owner()?;
    let requests = |from: usize| h.requests.lock().unwrap()[from..].to_vec();
    let mut previous = Uuid::nil();
    let conv = h.conversation(&user).await?;
    for text in ["bigoutput one", "bigoutput two", "readback"] {
        h.send(&user, conv, message(text)).await?;
        previous = h.wait_after(conv, previous).await?;
    }
    // Past the budget, the oldest results left; the model read one back by its id.
    let all = requests(0);
    let (asked, _) = all
        .iter()
        .rev()
        .find(|(body, _)| {
            body["messages"].to_string().contains("readback")
                && !body["messages"].to_string().contains("total_bytes")
        })
        .context("readback request")?;
    let moved = asked["messages"]
        .to_string()
        .matches("[Moved out of the context")
        .count();
    assert_eq!(moved, 2, "both earlier results moved");
    let (read, headers) = all.last().unwrap();
    let returned = read["messages"].as_array().unwrap().last().unwrap()["content"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        returned.contains("total_bytes") && returned.contains("aaaaaaaa"),
        "{returned}"
    );
    // Every request may compact past the threshold.
    assert_eq!(
        read["context_management"]["edits"][0]["type"],
        "compact_20260112"
    );
    assert_eq!(headers["anthropic-beta"], "compact-2026-01-12");
    let reply: String = sqlx::query_scalar("SELECT response_text FROM runs WHERE id=$1")
        .bind(previous)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(reply, "工具结果已用于回答");

    // `/compact` asks the provider; its block then leads the context.
    let from = h.requests.lock().unwrap().len();
    h.send(&user, conv, Command::Compact(Compact {})).await?;
    previous = h.wait_after(conv, previous).await?;
    let (compaction, headers) = requests(from)
        .last()
        .cloned()
        .context("compaction request")?;
    assert_eq!(compaction["compaction"]["type"], "summarize");
    assert!(compaction.get("tools").is_none() && compaction.get("context_management").is_none());
    assert_eq!(headers["anthropic-beta"], "compact-2026-09-04");
    let usage: (i64, i64) = sqlx::query_as(
        "SELECT input_tokens,output_tokens FROM usage_records WHERE run_id=$1 ORDER BY id DESC LIMIT 1",
    )
    .bind(previous)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(usage, (40, 6));
    let from = h.requests.lock().unwrap().len();
    h.send(&user, conv, message("after compaction")).await?;
    previous = h.wait_after(conv, previous).await?;
    let (next, _) = requests(from).last().cloned().context("next request")?;
    assert_eq!(
        next["messages"][0],
        json!({"role":"assistant","content":[{"type":"compaction","content":"压缩摘要"}]})
    );
    assert_eq!(next["messages"].as_array().unwrap().len(), 2);

    // A compaction the provider returns mid-reply is kept, and shows nothing.
    let from = h.requests.lock().unwrap().len();
    h.send(&user, conv, message("autocompact")).await?;
    previous = h.wait_after(conv, previous).await?;
    let reply: String = sqlx::query_scalar("SELECT response_text FROM runs WHERE id=$1")
        .bind(previous)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(reply, "压缩后继续");
    h.send(&user, conv, message("after automatic compaction"))
        .await?;
    h.wait_after(conv, previous).await?;
    let (next, _) = requests(from).last().cloned().context("request after it")?;
    let blocks = next["messages"].to_string();
    assert!(blocks.contains("自动压缩摘要"), "{blocks}");

    // A provider that cannot compact: the model summarizes, and the summary stands in.
    let other = h.conversation(&user).await?;
    let mut last = Uuid::nil();
    h.send(&user, other, message("nocompact hello")).await?;
    last = h.wait_after(other, last).await?;
    h.send(&user, other, Command::Compact(Compact {})).await?;
    last = h.wait_after(other, last).await?;
    let from = h.requests.lock().unwrap().len();
    h.send(&user, other, message("after summary")).await?;
    h.wait_after(other, last).await?;
    let (next, _) = requests(from)
        .last()
        .cloned()
        .context("request after summary")?;
    assert!(
        next["messages"][0]["content"]
            .as_str()
            .unwrap()
            .ends_with("摘要内容")
    );
    assert_eq!(next["messages"].as_array().unwrap().len(), 3);
    assert!(!next["messages"].to_string().contains("nocompact"));

    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

#[path = "platform/plugins.rs"]
mod plugins;

#[path = "platform/sso.rs"]
mod sso;

#[path = "platform/lifecycle.rs"]
mod lifecycle;

#[path = "platform/documents.rs"]
mod documents;

#[path = "platform/uploads.rs"]
mod uploads;

#[path = "platform/push.rs"]
mod push;
