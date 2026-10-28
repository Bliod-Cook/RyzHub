//! Real PostgreSQL, NATS and containerd; only the paid model upstream is a protocol fixture.
mod common;
use anyhow::{Context, ensure};
use axum::{Json, Router, body::Body, extract::State, http::HeaderMap, routing::post};
use common::{
    Harness, id, message,
    upstream::{DOT_PNG, Requests},
};
use prost::Message;
use ryzhub_proto::{command_envelope::Command, *};
use ryzhub_server::{
    app::App,
    config::{Config, ModelRoute, Upstream, reload::Running},
    llm::{gateway, keys},
    owner,
    store::{Lease, hash_token},
};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{process::Command as Process, task::JoinHandle};
use uuid::Uuid;

impl Harness {
    async fn new() -> anyhow::Result<Self> {
        Self::with(|_| {}).await
    }
    /// A harness whose configuration `configure` adjusts last.
    async fn with(configure: impl FnOnce(&mut Config)) -> anyhow::Result<Self> {
        // Plugin fixtures run on this machine; the app origin is served beside the API.
        let content = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", content.local_addr()?);
        let mut h = Self::start(|config| {
            config.llm.enabled = true;
            config.sandbox.enabled = true;
            // Sandboxes go when their run ends unless a test keeps them for reuse.
            config.sandbox.chat_idle_secs = 0;
            config.plugins.insecure_hosts = vec!["127.0.0.1".into()];
            config.plugins.client_url = "http://client.test".into();
            config.plugins.content_origin = origin;
            configure(config);
        })
        .await?;
        let apps = ryzhub_server::plugins::apps::content_router(h.app.clone());
        h.serve(content, apps);
        Ok(h)
    }
    async fn conversation(&self, user: &Value) -> anyhow::Result<Uuid> {
        let ctx = self.context(user).await?;
        Ok(self
            .create(&ctx, "Platform", "chat", json!({}), None)
            .await?
            .id)
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
    /// A gateway key for one Owner model call, as the chat engine issues it.
    async fn loop_key(&self, lease: &Lease, user: Uuid, run: Uuid) -> anyhow::Result<String> {
        keys::issue_loop_key(
            &self.app.store,
            lease,
            user,
            run,
            vec![self.app.config().llm.model.clone()],
        )
        .await
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and test gateway secrets"]
async fn gateway_passes_the_native_protocol_through_records_usage_and_fences_executions()
-> anyhow::Result<()> {
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
    owner::process_with_config(&h.app.store, &lease, &h.app.config()).await?;
    let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    let token = h.loop_key(&lease, uid, run).await?;
    let client = reqwest::Client::new();
    let body = json!({"model":"fixture-model","max_tokens":32,"stream":true,
        "messages":[{"role":"user","content":"hello"}],
        "system":[{"type":"text","text":"native","cache_control":{"type":"ephemeral"}}]});
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
    let usage = sqlx::query(
        "SELECT input_tokens,output_tokens,raw_usage,estimated,status
         FROM usage_records WHERE run_id=$1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(run)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(usage.get::<i64, _>("input_tokens"), 25);
    assert_eq!(usage.get::<i64, _>("output_tokens"), 8);
    assert!(!usage.get::<bool, _>("estimated"));
    assert_eq!(
        usage.get::<Value, _>("raw_usage")["cache_read_input_tokens"],
        12
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
#[ignore = "requires PostgreSQL/NATS and a model provider"]
async fn a_real_provider_answers_through_the_gateway_and_its_usage_is_recorded()
-> anyhow::Result<()> {
    let provider = ["RYZHUB_TEST_PROVIDER_URL", "RYZHUB_TEST_PROVIDER_MODEL"]
        .map(|name| std::env::var(name).unwrap_or_default());
    if provider.iter().any(String::is_empty)
        || std::env::var_os("RYZHUB_TEST_PROVIDER_KEY").is_none()
    {
        println!(
            "real provider skipped: RYZHUB_TEST_PROVIDER_URL, RYZHUB_TEST_PROVIDER_MODEL or \
             RYZHUB_TEST_PROVIDER_KEY is not set"
        );
        return Ok(());
    }
    let [url, model] = provider;
    let route = ModelRoute {
        id: model.clone(),
        protocol: "responses".into(),
        harnesses: vec![],
        upstreams: vec![Upstream {
            url,
            key_env: "RYZHUB_TEST_PROVIDER_KEY".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let h = Harness::with(|c| c.llm.models.push(route)).await?;
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
    owner::process_with_config(&h.app.store, &lease, &h.app.config()).await?;
    let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    let token = keys::issue_loop_key(
        &h.app.store,
        &lease,
        id(&user, "user_id"),
        run,
        vec![model.clone()],
    )
    .await?;
    let response: Value = reqwest::Client::new()
        .post(format!("{}/v1/responses", h.gateway))
        .bearer_auth(token)
        .json(&json!({"model":model,"input":"Reply with the single word OK."}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(response["status"], "completed", "{response}");
    let usage: (String, i64, bool) = sqlx::query_as(
        "SELECT status,input_tokens,estimated FROM usage_records WHERE run_id=$1 AND model=$2",
    )
    .bind(run)
    .bind(&model)
    .fetch_one(&h.db)
    .await?;
    assert!(
        usage.0 == "completed" && usage.1 > 0 && !usage.2,
        "{usage:?}"
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
    let tool_item: (String, String) = sqlx::query_as(
        "SELECT i.state,i.text FROM tool_calls t
         JOIN items i ON i.id=t.item_id AND i.org_id=t.org_id WHERE t.run_id=$1",
    )
    .bind(run)
    .fetch_one(&h.db)
    .await?;
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
    let consumed: bool = sqlx::query_scalar(
        "SELECT bool_and(consumed_step IS NOT NULL)
         FROM pending_inputs WHERE run_id IN (SELECT id FROM runs
         WHERE conversation_id=$1)",
    )
    .bind(conv2)
    .fetch_one(&h.db)
    .await?;
    assert!(consumed);
    h.send(
        &user,
        conv2,
        Command::SideQuery(SideQuery {
            text: "旁问".into(),
        }),
    )
    .await?;
    // Wait for the side query's own run, not the previous completed one.
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
    // The paused turn is re-sent with its search, without the steer that arrived meanwhile.
    let resumed = requests[1]["messages"].to_string();
    assert!(resumed.contains("opaque-search") && !resumed.contains("补充输入"));
    // The steer follows the finished turn, citations and all.
    let after = requests[2]["messages"].as_array().unwrap();
    assert_eq!(after.last().unwrap()["content"], "补充输入");
    let turn = after[after.len() - 2].to_string();
    assert!(turn.contains("先搜索一下") && turn.contains("opaque-index"));
    // The search splits the paused step's reply where it happened; its citations are kept.
    let items: Vec<(String, String, Option<Value>)> = sqlx::query_as(
        "SELECT kind,state,detail FROM items WHERE run_id=$1 AND kind IN ('assistant','tool')
         ORDER BY first_seq",
    )
    .bind(run)
    .fetch_all(&h.db)
    .await?;
    let shape: Vec<(&str, &str)> = items
        .iter()
        .map(|(kind, state, _)| (kind.as_str(), state.as_str()))
        .collect();
    assert_eq!(
        shape,
        [
            ("assistant", "completed"),
            ("tool", "completed"),
            ("assistant", "completed"),
            ("assistant", "completed"),
            ("assistant", "completed"),
        ]
    );
    assert_eq!(
        items[1].2.as_ref().unwrap()["kind"]["Tool"]["name"],
        "web_search"
    );
    let cited = items[3].2.as_ref().unwrap()["kind"]["Answer"]["citations"].clone();
    assert_eq!(cited[0]["url"], "https://example.com/a", "{cited}");
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
    // The cited page showed while the reply was still streaming.
    let payloads: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT payload FROM events WHERE kind='item_upserted'")
            .fetch_all(&h.db)
            .await?;
    let streamed_citations = payloads
        .iter()
        .filter_map(|p| match EventEnvelope::decode(p.as_slice()).ok()?.event? {
            event_envelope::Event::ItemUpserted(item) => Some(item),
            _ => None,
        })
        .any(|item| {
            item.state == "streaming"
                && item.run_id == run.to_string()
                && matches!(
                    item.detail.as_ref().and_then(|d| d.kind.as_ref()),
                    Some(item_detail::Kind::Answer(answer)) if !answer.citations.is_empty()
                )
        });
    assert!(streamed_citations);
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
        !h.app.config().sandbox.image.is_empty(),
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
    assert_eq!(files.len(), 2, "{files:?}");
    let report = &files[0]["file"];
    assert_eq!(report["name"], "report.txt");
    assert_eq!(report["size"], 16);
    // What cannot be handed over says why.
    assert_eq!(files[1]["error"], "FILE_NOT_FOUND");
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
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS, secrets and scripts/start-execution-node.sh"]
async fn library_files_reach_models_and_sandboxes_until_deleted() -> anyhow::Result<()> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let h = Harness::new().await?;
    ensure!(
        !h.app.config().sandbox.image.is_empty(),
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
    // The model gets the image, the document and the text with the message.
    let sent = h
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|(body, _)| body["messages"].to_string())
        .find(|messages| messages.contains(DOT_PNG))
        .context("no request carried the files")?;
    let document = b64.encode(&pdf);
    for part in [document.as_str(), "a,b\\n1,2", "attachfiles 看看"] {
        assert!(sent.contains(part), "{part} missing from {sent}");
    }
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
         AND (coalesce(s.request_messages::text,'') LIKE '%'||$2||'%'
           OR coalesce(s.input_delta::text,'') LIKE '%'||$2||'%')",
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
            "SELECT id FROM runs WHERE conversation_id=$1
             AND state='completed' ORDER BY started_at",
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
    assert!(!request.contains(DOT_PNG), "a deleted image was sent again");
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
        !request.contains("a,b\\n1,2"),
        "deleted text was sent again"
    );
    // Content goes with the last entry that holds it.
    let sha256: String = sqlx::query_scalar("SELECT sha256 FROM attachments WHERE id=$1")
        .bind(Uuid::parse_str(&image_id)?)
        .fetch_one(&h.db)
        .await?;
    let object = std::path::Path::new(&h.app.config().work.storage_root)
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
        !h.app.config().sandbox.image.is_empty(),
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
         AND (coalesce(s.request_messages::text,'') LIKE '%'||$2||'%'
           OR coalesce(s.input_delta::text,'') LIKE '%'||$2||'%')",
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
    owner::process_with_config(&h.app.store, &lease, &h.app.config()).await?;
    let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    let sandbox = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle)
         VALUES($1,$2,$3,$4,7,1,'running','{}')",
    )
    .bind(sandbox)
    .bind(org)
    .bind(conv)
    .bind(run)
    .execute(&h.db)
    .await?;
    let request = |lease_epoch| keys::SandboxKeyRequest {
        org_id: org,
        user_id: id(&user, "user_id"),
        conversation_id: conv,
        run_id: run,
        sandbox_id: sandbox,
        lease_epoch,
        models: vec!["fixture-model".into()],
    };
    let token = keys::issue_sandbox_key(&h.app.store, request(7))
        .await?
        .token;
    let client = reqwest::Client::new();
    let body = json!({"model":"fixture-model","max_tokens":32,"stream":true,
        "messages":[{"role":"user","content":"hello"}]});
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
    // A delta of a kind the gateway does not know passes through; the stream still completes.
    let mut future = body.clone();
    future["messages"][0]["content"] = json!("futuredelta");
    send(token.clone(), future)
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let usage: (bool, String, i64, i64) = sqlx::query_as(
        "SELECT estimated,status,input_tokens,output_tokens
         FROM usage_records WHERE run_id=$1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(run)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(usage, (false, "completed".into(), 25, 8));
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
    let usage: (bool, String) = sqlx::query_as(
        "SELECT estimated,status FROM usage_records WHERE run_id=$1
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(run)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(usage, (true, "interrupted".into()));
    let mut config = (*h.app.config()).clone();
    config.llm.requests_per_minute_key = 4;
    let limited = App::new(
        h.app.store.clone(),
        h.app.nats.clone(),
        Running::new(config, None),
    )?;
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
#[ignore = "requires PostgreSQL/NATS, secrets and scripts/start-execution-node.sh"]
async fn a_tool_outlives_owner_and_host_restarts_once_and_its_sandbox_is_cleaned_up()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    ensure!(
        !h.app.config().sandbox.image.is_empty(),
        "execution image missing"
    );
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let mut owner = h.owner()?;
    // The command marks the workspace, waits, then calls the model through the guest's proxy.
    h.send(&user, conv, message("proxyexec")).await?;
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
    // The Owner dies once the command is in the guest, then the host restarts under it.
    tokio::time::sleep(Duration::from_millis(150)).await;
    owner.kill().await?;
    owner.wait().await?;
    let node = |script: String| async move {
        Process::new("docker")
            .args(["exec", "ryzhub-execution", "sh", "-c", &script])
            .output()
            .await
    };
    let restarted = node("kill \"$(cat /state/host.pid)\"".into()).await?;
    ensure!(restarted.status.success(), "host restart failed");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let status: String = sqlx::query_scalar("SELECT status FROM tool_calls WHERE run_id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(status, "started");
    // A new Owner takes the finished result from the guest's journal: it ran once.
    let mut owner = h.owner()?;
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
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM guest_events WHERE sandbox_id=$1")
        .bind(sandbox)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(events, 1);
    // On the quota volume when the node has one.
    let marker = format!(
        "org/{}/workspaces/{conv}/marker",
        user["org_id"].as_str().unwrap()
    );
    let read = node(format!(
        "cat /state/volume/{marker} 2>/dev/null || cat /state/{marker}"
    ))
    .await?;
    assert!(read.status.success());
    assert_eq!(read.stdout, b"once");
    let usages: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_records WHERE run_id=$1 AND virtual_key_id IS NOT NULL
           AND status='completed' AND NOT estimated",
    )
    .bind(run)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(usages, 1);
    owner.kill().await?;
    owner.wait().await?;
    // A stop whose acknowledgement was lost after the run's end was published.
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
    let gone = node(format!(
        "! test -e /state/sandboxes/{sandbox} && ! test -e /state/volume/sandboxes/{sandbox} \
         && ! grep -q /sandboxes/{sandbox}/ /proc/mounts"
    ))
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
async fn organization_credentials_are_sealed_revocable_and_kept_within_the_organization()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let outsider = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    let client = reqwest::Client::new();
    let token = user["token"].as_str().unwrap();
    let credential: Value = client
        .post(format!("{}/orgs/{org}/credentials", h.base))
        .bearer_auth(token)
        .json(&json!({"name":"test","organization":true,"api_key":"org-fixture-key"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    h.send(&user, conv, message("hello")).await?;
    let lease = h
        .app
        .store
        .claim(org, conv, Uuid::now_v7(), 60_000)
        .await?
        .unwrap();
    owner::process_with_config(&h.app.store, &lease, &h.app.config()).await?;
    let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    let virtual_token = h.loop_key(&lease, id(&user, "user_id"), run).await?;
    let body = json!({"model":"fixture-model","stream":true,"max_tokens":32,
        "messages":[{"role":"user","content":"hello"}]});
    let upstream_key = || async {
        client
            .post(format!("{}/v1/messages", h.gateway))
            .bearer_auth(&virtual_token)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let key = h.requests.lock().unwrap().last().unwrap().1["x-api-key"].clone();
        anyhow::Ok(key)
    };
    assert_eq!(upstream_key().await?, "org-fixture-key");
    // The key lives envelope-encrypted in a secret; the credential row holds only its id.
    let (ciphertext, wrapped): (Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT s.ciphertext,s.wrapped_dek FROM model_credentials c
         JOIN secrets s ON s.org_id=c.org_id AND s.id=c.secret_id WHERE c.id=$1",
    )
    .bind(id(&credential, "id"))
    .fetch_one(&h.db)
    .await?;
    for stored in [&ciphertext, &wrapped] {
        assert!(!stored.windows(15).any(|b| b == b"org-fixture-key"));
    }
    let secrets = || {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM secrets WHERE org_id=$1")
            .bind(org)
            .fetch_one(&h.db)
    };
    assert_eq!(secrets().await?, 1);
    client
        .delete(format!(
            "{}/orgs/{org}/credentials/{}",
            h.base,
            credential["id"].as_str().unwrap()
        ))
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;
    // Revocation deletes the key outright; requests fall back to the platform's.
    assert_eq!(secrets().await?, 0);
    assert_eq!(upstream_key().await?, "local-fixture-key");
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
async fn vault_key_rotation_rewraps_credential_secrets() -> anyhow::Result<()> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let h = Harness::new().await?;
    let user = h.user().await?;
    let (org, owner) = (id(&user, "org_id"), id(&user, "user_id"));
    let credential: Value = reqwest::Client::new()
        .post(format!("{}/orgs/{org}/credentials", h.base))
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"name":"personal","api_key":"personal-fixture-key"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let resolve = |app: App| async move {
        let mut tx = app.store.tx(org).await?;
        let policy = ryzhub_domain::Policy::default();
        let resolved = ryzhub_server::llm::credentials::member_key(
            &app,
            &mut tx,
            org,
            owner,
            &policy,
            "anthropic",
        )
        .await?;
        tx.commit().await?;
        anyhow::Ok(resolved)
    };
    let expected = Some((id(&credential, "id"), "personal-fixture-key".to_owned()));
    assert_eq!(resolve(h.app.clone()).await?, expected);
    let migrate = ryzhub_server::vault::migrate;
    assert_eq!(
        migrate(&h.db, &h.app.config().llm).await?,
        0,
        "nothing to rewrap"
    );

    // Rotate: key 1 becomes current and key 0 is retired. Rewrapping makes key 0 unnecessary.
    // SAFETY: integration tests run on one thread (--test-threads=1).
    unsafe {
        std::env::set_var(
            "RYZHUB_TEST_VAULT_KEY_NEXT",
            URL_SAFE_NO_PAD.encode([5u8; 32]),
        )
    };
    let mut rotated = (*h.app.config()).clone();
    rotated.llm.vault_key_id = 1;
    rotated.llm.vault_key_env = "RYZHUB_TEST_VAULT_KEY_NEXT".into();
    rotated.llm.vault_retired_keys = [("0".into(), "RYZHUB_TEST_VAULT_KEY".into())].into();
    assert_eq!(migrate(&h.db, &rotated.llm).await?, 1);
    let kek: i16 = sqlx::query_scalar(
        "SELECT s.kek_id FROM secrets s JOIN model_credentials c ON c.secret_id=s.id WHERE c.id=$1",
    )
    .bind(id(&credential, "id"))
    .fetch_one(&h.db)
    .await?;
    assert_eq!(kek, 1);
    rotated.llm.vault_retired_keys.clear();
    let app = App::new(
        h.app.store.clone(),
        h.app.nats.clone(),
        Running::new(rotated, None),
    )?;
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
                "SELECT idle_until>now() FROM sandboxes
                 WHERE conversation_id=$1 AND state='running'",
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
        sqlx::query(
            "UPDATE sandboxes SET idle_until=now()-interval '1 second'
             WHERE id=$1 AND idle_until IS NOT NULL",
        )
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
                "rm -rf /state/org/{org}/workspaces/{conv} \
                 /state/volume/org/{org}/workspaces/{conv}"
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
            "SELECT id FROM sandboxes WHERE conversation_id=$1
             AND state='running' AND idle_until IS NOT NULL",
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
        "UPDATE channels SET writer_epoch=writer_epoch+40
         WHERE conversation_id=$1 RETURNING writer_epoch",
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
        "UPDATE sandboxes
         SET persistent=true,state='lost',node_lease_until=now()-interval '2 days'
         WHERE id=$1",
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
            "SELECT EXISTS(SELECT 1 FROM workspace_heads h
               JOIN checkpoints c ON c.id=h.checkpoint_id
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
            &format!(
                "! test -e /state/volume/org/{org}/workspaces/{conv} \
                 && ! test -e /state/org/{org}/workspaces/{conv}"
            ),
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

    // A suspended Work sandbox is destroyed once its grace period is over; a running one whose
    // checkpoints fall behind is reported once.
    let other = h.conversation(&user).await?;
    h.send(&user, other, message("hello")).await?;
    let run = h.wait_state(other, "completed").await?;
    let (suspended, running) = (Uuid::now_v7(), Uuid::now_v7());
    for (sandbox, state, age) in [
        (suspended, "suspended", "2 days"),
        (running, "running", "1 hour"),
    ] {
        sqlx::query(
            "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,
               handle,persistent,node_lease_until,created_at)
             VALUES($1,$2,$3,$4,1,1,$5,'{}',true,now()-$6::interval,now()-$6::interval)",
        )
        .bind(sandbox)
        .bind(org)
        .bind(other)
        .bind(run)
        .bind(state)
        .bind(age)
        .execute(&h.db)
        .await?;
    }
    for _ in 0..2 {
        ryzhub_server::sandbox::reclaim_once(&h.app, &worker).await?;
    }
    let state: String = sqlx::query_scalar("SELECT state FROM sandboxes WHERE id=$1")
        .bind(suspended)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(state, "deleted");
    let reports: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs WHERE org_id=$1
         AND action='workspace.checkpoint_stale' AND resource=$2",
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
async fn a_dependency_layer_mounts_and_leaves_the_node_once_unused() -> anyhow::Result<()> {
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
    let h = Harness::with(|config| config.sandbox.deps = deps.clone()).await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
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
    let mut keep = (*h.app.config()).clone();
    keep.sandbox.image_gc_after_secs = 0;
    let app = App::new(
        h.app.store.clone(),
        h.app.nats.clone(),
        Running::new(keep.clone(), None),
    )?;
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
    // Once configuration no longer names it and nothing uses it, it leaves the node.
    keep.sandbox.deps.clear();
    let app = App::new(
        h.app.store.clone(),
        h.app.nats.clone(),
        Running::new(keep, None),
    )?;
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
    owner::process_with_config(&h.app.store, &lease, &h.app.config()).await?;
    let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    let token = h.loop_key(&lease, uid, run).await?;
    let slow = json!({"model":"fixture-model","max_tokens":32,"stream":true,
        "messages":[{"role":"user","content":"slow"}]});
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", h.gateway))
        .bearer_auth(token)
        .json(&slow)
        .send()
        .await?
        .error_for_status()?;
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
        let choice = json!({"index":0,"message":{"role":"assistant","content":"你好"},
            "finish_reason":"stop"});
        let usage = json!({"prompt_tokens":9,"completion_tokens":2});
        let whole = json!({"id":"c2","object":"chat.completion","choices":[choice],"usage":usage});
        return Json(whole).into_response();
    }
    let chunk =
        |choices: Value| json!({"id":"c1","object":"chat.completion.chunk","choices":choices});
    let mut last = chunk(json!([]));
    last["usage"] = json!({"prompt_tokens":11,"completion_tokens":3,"total_tokens":14});
    let chunks = [
        chunk(json!([{"index":0,"delta":{"role":"assistant","content":"你好"}}])),
        chunk(json!([{"index":0,"delta":{},"finish_reason":"stop"}])),
        last,
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
        let candidate = json!({"content":{"parts":[{"text":"你好"}],"role":"model"},
            "finishReason":"STOP"});
        let usage = json!({"promptTokenCount":20,"candidatesTokenCount":4,"thoughtsTokenCount":6});
        return Json(json!({"candidates":[candidate],"usageMetadata":usage})).into_response();
    }
    let part = |text: &str| json!({"content":{"parts":[{"text":text}],"role":"model"}});
    let mut done = part("好");
    done["finishReason"] = json!("STOP");
    let chunks = [
        json!({"candidates":[part("你")],
            "usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":1}}),
        json!({"candidates":[done],
            "usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":2,
                "thoughtsTokenCount":5}}),
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
async fn gateway_speaks_chat_completions_and_gemini_fails_over_and_caps_reasoning()
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
    owner::process_with_config(&h.app.store, &lease, &h.app.config()).await?;
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

    // A reasoning cap refuses requests above it, in each protocol's terms.
    sqlx::query("UPDATE org_policies SET policy='{\"max_reasoning\":\"low\"}' WHERE org_id=$1")
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
    let mut deep = json!({"model":"fixture-model","max_tokens":32,"stream":true,
        "messages":[{"role":"user","content":"hello"}]});
    deep["thinking"] = json!({"type":"enabled","budget_tokens":30000});
    assert_eq!(post("/v1/messages", deep).await?.status(), 403);
    let mut low = chat.clone();
    low["reasoning_effort"] = json!("low");
    post("/v1/chat/completions", low)
        .await?
        .error_for_status()?
        .bytes()
        .await?;
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
    assert!(
        asked["messages"]
            .to_string()
            .contains("[Moved out of the context"),
        "nothing moved"
    );
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
        "SELECT input_tokens,output_tokens FROM usage_records
         WHERE run_id=$1 ORDER BY id DESC LIMIT 1",
    )
    .bind(previous)
    .fetch_one(&h.db)
    .await?;
    assert_eq!(usage, (40, 6));
    let from = h.requests.lock().unwrap().len();
    h.send(&user, conv, message("after compaction")).await?;
    previous = h.wait_after(conv, previous).await?;
    let (next, _) = requests(from).last().cloned().context("next request")?;
    let context = next["messages"].to_string();
    assert!(
        context.contains("压缩摘要") && !context.contains("bigoutput"),
        "{context}"
    );

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
    let context = next["messages"].to_string();
    assert!(
        context.contains("摘要内容") && !context.contains("nocompact"),
        "{context}"
    );

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
