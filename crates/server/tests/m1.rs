//! Real PostgreSQL, NATS and containerd; only the paid model upstream is a protocol fixture.
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
    config::Config,
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
    // Re-sent assistant content continues a paused turn.
    let continued = body["messages"].as_array().unwrap().last().unwrap()["role"] == "assistant";
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
    } else if !result && body.get("tools").is_some() && prompt.contains("exec") {
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
            start = json!({"type":"text","text":""});
        }
        if block["type"] == "server_tool_use" {
            start["input"] = json!({});
        }
        events.push(json!({"type":"content_block_start","index":index,"content_block":start}));
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
        config.llm.model = "fixture-model".into();
        config.llm.upstream_url = format!("http://{}", upstream_listener.local_addr()?);
        config.llm.gateway_url = format!("http://{}", gateway_listener.local_addr()?);
        config.llm.platform_key_env = "RYZHUB_TEST_UPSTREAM_KEY".into();
        config.llm.vault_key_env = "RYZHUB_TEST_VAULT_KEY".into();
        config.sandbox.enabled = true;
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
        let tasks = vec![
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
            .create_conversation(&ctx, "M1", "chat")
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
    let encrypted: Vec<u8> =
        sqlx::query_scalar("SELECT encrypted_key FROM model_credentials WHERE id=$1")
            .bind(id(&credential, "id"))
            .fetch_one(&h.db)
            .await?;
    assert!(!encrypted.windows(20).any(|b| b == b"personal-fixture-key"));
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
    let run = h.wait_state(conv, "requires_action").await?;
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
            "ryzhub-m1-execution",
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
    let marker = format!(
        "/state/org/{}/workspaces/{conv}/marker",
        user["org_id"].as_str().unwrap()
    );
    let output = Process::new("docker")
        .args(["exec", "ryzhub-m1-execution", "cat", &marker])
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
    let run = h.wait_state(conv, "requires_action").await?;
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
    let output = Process::new("docker")
        .args([
            "exec",
            "ryzhub-m1-execution",
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
    let journal = Process::new("docker")
        .args([
            "exec",
            "ryzhub-m1-execution",
            "cat",
            &format!("/state/sandboxes/{sandbox}/control/journal.json"),
        ])
        .output()
        .await?;
    ensure!(journal.status.success(), "guest journal missing");
    let journal = String::from_utf8(journal.stdout)?;
    assert!(!journal.contains("local-fixture-key") && !journal.contains("eyJhbGci"));
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
    let mut stopped = false;
    for _ in 0..300 {
        stopped = sqlx::query_scalar("SELECT state='stopped' FROM sandboxes WHERE id=$1")
            .bind(sandbox)
            .fetch_one(&h.db)
            .await?;
        if stopped {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        stopped,
        "terminal-only conversation must be rediscovered for cleanup"
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
