//! Real PostgreSQL, NATS and containerd; only the paid model upstream is a protocol fixture.
mod common;
use anyhow::{Context, ensure};
use common::{Harness, guest_exec, id, message};
use reqwest::Method;
use ryzhub_proto::{command_envelope::Command, *};
use ryzhub_server::config::ModelRoute;
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

/// OpenCode over ACP, through its Anthropic provider and the guest's model proxy.
fn opencode_profile() -> ryzhub_execution::AcpProfile {
    let config = json!({
        "$schema": "https://opencode.ai/config.json",
        "autoupdate": false,
        "share": "disabled",
        "enabled_providers": ["anthropic"],
        "model": "anthropic/{model}",
        "small_model": "anthropic/{model}",
        "instructions": ["{instructions}"],
        "provider": {"anthropic": {
            "options": {"baseURL": "{proxy}/v1", "apiKey": "guest-proxy"},
            "models": "{models}",
        }},
        "permission": {"bash": "ask", "edit": "ask", "webfetch": "ask"},
    })
    .to_string()
    .replace("\"{models}\"", "{models}");
    let mut env: std::collections::BTreeMap<String, String> = [
        "OPENCODE_DISABLE_AUTOUPDATE",
        "OPENCODE_DISABLE_MODELS_FETCH",
        "OPENCODE_DISABLE_LSP_DOWNLOAD",
        "OPENCODE_DISABLE_DEFAULT_PLUGINS",
        "OPENCODE_DISABLE_SHARE",
        "OPENCODE_DISABLE_CLAUDE_CODE",
    ]
    .into_iter()
    .map(|name| (name.to_owned(), "1".to_owned()))
    .collect();
    env.insert("OPENCODE_CONFIG_CONTENT".into(), config);
    ryzhub_execution::AcpProfile {
        name: "opencode".into(),
        label: "OpenCode".into(),
        command: "/usr/local/bin/opencode".into(),
        args: vec!["acp".into()],
        env,
        model_option: "anthropic/{model}".into(),
        protocol: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        version: "1.19.0".into(),
    }
}

impl Harness {
    async fn new() -> anyhow::Result<Self> {
        Self::start(|config| {
            common::work(config);
            config.work.claude_model = "claude-sonnet-4-6".into();
            config.llm.models = vec![
                ModelRoute {
                    id: "gpt-5.4".into(),
                    protocol: "responses".into(),
                    harnesses: vec!["chat".into()],
                    ..Default::default()
                },
                ModelRoute {
                    id: "claude-opus-4-6".into(),
                    protocol: "anthropic".into(),
                    harnesses: vec!["claude".into(), "chat".into(), "opencode".into()],
                    ..Default::default()
                },
                ModelRoute {
                    id: "gpt-5.2".into(),
                    protocol: "responses".into(),
                    harnesses: vec!["codex".into()],
                    ..Default::default()
                },
            ];
            config.harness.acp = vec![opencode_profile()];
            config.policy.allow_full_auto = Some(false);
            config.sandbox.image = std::env::var("TEST_HARNESS_IMAGE").unwrap_or_else(|_| {
                let image = common::workspace().join("var/execution/harness-image.txt");
                std::fs::read_to_string(image)
                    .unwrap_or_default()
                    .trim()
                    .into()
            });
        })
        .await
    }
    async fn configured(
        &self,
        user: &Value,
        mode: &str,
        settings: Value,
        project: Option<Uuid>,
    ) -> anyhow::Result<Uuid> {
        let ctx = self.context(user).await?;
        Ok(self
            .create(&ctx, "Harness", mode, settings, project)
            .await?
            .id)
    }
    /// Send `command` and wait until the Owner accepts it.
    async fn submit(&self, user: &Value, conv: Uuid, command: Command) -> anyhow::Result<Uuid> {
        use prost::Message;
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
    /// Send `text` and wait for its run to complete and the workspace to settle.
    async fn turn(&self, user: &Value, conv: Uuid, text: &str) -> anyhow::Result<Uuid> {
        self.submit(user, conv, message(text)).await?;
        let run = self.wait_state(conv, "completed").await?;
        self.idle(conv).await?;
        Ok(run)
    }
    async fn json(
        &self,
        user: &Value,
        path: &str,
        method: Method,
        body: Value,
    ) -> anyhow::Result<Value> {
        let path = path.replace("{org}", &id(user, "org_id").to_string());
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
    /// Wait until no workspace job of `conv` is queued or running.
    async fn idle(&self, conv: Uuid) -> anyhow::Result<()> {
        for _ in 0..1000 {
            let busy: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM workspace_jobs
                   WHERE conversation_id=$1 AND state IN ('queued','running'))",
            )
            .bind(conv)
            .fetch_one(&self.db)
            .await?;
            if !busy {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("WORKSPACE_TIMEOUT")
    }
    /// Fork `conv` at `item`; the new conversation.
    async fn fork(&self, user: &Value, conv: Uuid, item: Uuid) -> anyhow::Result<Uuid> {
        let fork = Fork {
            item_id: item.to_string(),
        };
        let cid = self.submit(user, conv, Command::Fork(fork)).await?;
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
    /// Send `text`, whose command waits for an approval; the run and its action.
    async fn asking(&self, user: &Value, conv: Uuid, text: &str) -> anyhow::Result<(Uuid, Uuid)> {
        self.submit(user, conv, message(text)).await?;
        let run = self.wait_state(conv, "requires_action").await?;
        Ok((run, self.action(run).await?.0))
    }
    /// Allow the pending action of `run`.
    async fn allow(&self, user: &Value, conv: Uuid, run: Uuid) -> anyhow::Result<()> {
        let (action, epoch) = self.action(run).await?;
        let decision = DecideApproval {
            action_id: action.to_string(),
            run_epoch: epoch as u64,
            decision: "allow".into(),
        };
        self.submit(user, conv, Command::DecideApproval(decision))
            .await?;
        Ok(())
    }
    /// Run `cmd` in the conversation's running sandbox.
    async fn exec(&self, conv: Uuid, op: &str, cmd: &str) -> anyhow::Result<Value> {
        let (sandbox, epoch): (Uuid, i64) = sqlx::query_as(
            "SELECT id,exec_epoch FROM sandboxes WHERE conversation_id=$1 AND state='running'",
        )
        .bind(conv)
        .fetch_one(&self.db)
        .await?;
        let host = std::path::Path::new(&self.app.config.sandbox.host_socket);
        guest_exec(host, sandbox, epoch, op, cmd).await
    }
    /// The model's last request.
    fn last_request(&self) -> Value {
        self.requests.lock().unwrap().last().unwrap().0.clone()
    }
}

// What every Work harness does in a conversation.

/// A steer sent while the turn is still starting is handed to it; settings changed meanwhile
/// apply from the next run, whose model request carries them.
async fn steer_then_configure(
    h: &Harness,
    user: &Value,
    conv: Uuid,
    harness: &str,
    model: &str,
) -> anyhow::Result<()> {
    h.submit(user, conv, message("slow hello")).await?;
    let run = h.wait_state(conv, "running").await?;
    let steer = Steer {
        text: "second queued instruction".into(),
        ..Default::default()
    };
    h.submit(user, conv, Command::Steer(steer)).await?;
    let settings = json!({"model":model,"reasoning":"high","plan":true}).to_string();
    let update = UpdateRunConfig {
        config_json: settings,
    };
    h.submit(user, conv, Command::UpdateRunConfig(update))
        .await?;
    let active: Value = sqlx::query_scalar("SELECT run_config FROM runs WHERE id=$1")
        .bind(run)
        .fetch_one(&h.db)
        .await?;
    ensure!(
        active["model"] != model && active["plan"] == false,
        "settings must not alter the active run"
    );
    h.wait_state(conv, "completed").await?;
    let steer: String =
        sqlx::query_scalar("SELECT state FROM items WHERE run_id=$1 AND kind='steer'")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
    ensure!(steer == "handed", "{harness} steer: {steer}");
    if harness != "codex" {
        let turns: i64 = sqlx::query_scalar("SELECT count(*) FROM native_turns WHERE run_id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
        ensure!(turns == 2, "{harness} queues a second native turn: {turns}");
    }
    h.idle(conv).await?;
    let next = h.turn(user, conv, "plan a change").await?;
    let settings: Value = sqlx::query_scalar("SELECT run_config FROM runs WHERE id=$1")
        .bind(next)
        .fetch_one(&h.db)
        .await?;
    ensure!(
        settings["plan"] == true && settings["reasoning"] == "high" && settings["model"] == model
    );
    ensure!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .any(|(b, _)| b["model"] == model),
        "the native model switch must reach the upstream"
    );
    // Out of Plan mode, commands run.
    let update = UpdateRunConfig {
        config_json: json!({"plan":false}).to_string(),
    };
    h.submit(user, conv, Command::UpdateRunConfig(update))
        .await?;
    Ok(())
}

/// A reply hands over the workspace file it links to.
async fn hand_over_a_file(
    h: &Harness,
    user: &Value,
    conv: Uuid,
    harness: &str,
) -> anyhow::Result<()> {
    h.exec(
        conv,
        "share-report",
        "mkdir -p /workspace/share && printf 'work report' > /workspace/share/report.txt",
    )
    .await?;
    let shared = h.turn(user, conv, "HARNESS_SHARE").await?;
    let detail: Value = sqlx::query_scalar(
        "SELECT detail FROM items WHERE run_id=$1 AND kind='assistant' AND state='completed'
           AND text LIKE '%sandbox:%'",
    )
    .bind(shared)
    .fetch_one(&h.db)
    .await?;
    let file = &detail["kind"]["Answer"]["files"][0];
    ensure!(
        file["link"] == "/workspace/share/report.txt"
            && file["file"]["name"] == "report.txt"
            && file["file"]["size"] == 11,
        "{harness} hand-over: {detail}"
    );
    Ok(())
}

/// The latest reply is a fork point the fork continues from.
async fn fork_and_continue(h: &Harness, user: &Value, conv: Uuid) -> anyhow::Result<()> {
    let latest: Uuid = sqlx::query_scalar(
        "SELECT id FROM items WHERE conversation_id=$1 AND kind='assistant' AND state='completed'
         ORDER BY last_seq DESC LIMIT 1",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    let fork = h.fork(user, conv, latest).await?;
    h.turn(user, fork, "hello fork").await?;
    Ok(())
}

/// Compacting and restoring the workspace keep the session going, over a read-only project
/// mount; a cancelled turn stops; an expired project takes the workspace and its checkpoints.
async fn compact_restore_cancel_and_expire(
    h: &Harness,
    user: &Value,
    conv: Uuid,
    harness: &str,
) -> anyhow::Result<()> {
    let org = id(user, "org_id");
    h.submit(user, conv, Command::Compact(Compact {})).await?;
    h.wait_state(conv, "completed").await?;
    h.idle(conv).await?;
    let workspace = format!("/orgs/{{org}}/conversations/{conv}/workspace");
    let job = h
        .json(user, &workspace, Method::POST, json!({"kind":"restore"}))
        .await?;
    let job = id(&job, "id");
    for _ in 0..1000 {
        let (state, result): (String, Option<Value>) =
            sqlx::query_as("SELECT state,result FROM workspace_jobs WHERE id=$1")
                .bind(job)
                .fetch_one(&h.db)
                .await?;
        if state == "completed" {
            break;
        }
        ensure!(state != "failed", "{harness} restore failed: {result:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    h.turn(user, conv, "hello restored").await?;
    let write = h
        .exec(conv, "project-read-only", "printf bad > /project/nope")
        .await?;
    ensure!(write["exit_code"] != 0, "/project must be read-only");
    h.submit(user, conv, message("slow cancel")).await?;
    h.wait_state(conv, "running").await?;
    h.submit(user, conv, Command::Cancel(Cancel {})).await?;
    h.wait_state(conv, "cancelled").await?;
    h.idle(conv).await?;
    let project = json!({"name":"Work retention","memory_mode":"default"});
    let project = id(
        &h.json(user, "/orgs/{org}/projects", Method::POST, project)
            .await?,
        "id",
    );
    let path = format!("/orgs/{{org}}/conversations/{conv}/project");
    h.json(user, &path, Method::POST, json!({"project_id":project}))
        .await?;
    let path = format!("/orgs/{{org}}/projects/{project}");
    h.json(user, &path, Method::DELETE, json!({})).await?;
    sqlx::query("UPDATE projects SET deleted_at=now()-interval '31 days' WHERE id=$1")
        .bind(project)
        .execute(&h.db)
        .await?;
    ensure!(
        ryzhub_server::projects::collect(&h.app, org, project).await?,
        "an expired Work project reclaims its sandbox and checkpoints"
    );
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM checkpoints WHERE conversation_id=$1 AND state<>'deleted'",
    )
    .bind(conv)
    .fetch_one(&h.db)
    .await?;
    ensure!(live == 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires actual Codex in the harness image"]
async fn a_codex_session_steers_approves_once_hands_over_forks_restores_and_cancels()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let mut owner = h.owner()?;
    let outcome = async {
        let conv = h
            .configured(&user, "work", json!({"harness":"codex"}), None)
            .await?;
        steer_then_configure(&h, &user, conv, "codex", "gpt-5.2").await?;
        // The approved command runs once, and only once allowed.
        let (run, _) = h.asking(&user, conv, "native-exec").await?;
        let marker = "/workspace/approval-marker";
        let before = h
            .exec(conv, "before-approval", &format!("test ! -f {marker}"))
            .await?;
        ensure!(before["exit_code"] == 0, "ran before the approval");
        h.allow(&user, conv, run).await?;
        h.wait_state(conv, "completed").await?;
        h.idle(conv).await?;
        let after = h
            .exec(conv, "after-approval", &format!("cat {marker}"))
            .await?;
        ensure!(after["stdout_head"] == "native-once", "{after}");
        let tool: String = sqlx::query_scalar(
            "SELECT text FROM items WHERE run_id=$1 AND kind='tool' AND state='completed'
             ORDER BY last_seq DESC LIMIT 1",
        )
        .bind(run)
        .fetch_one(&h.db)
        .await?;
        ensure!(tool.contains("native-once"), "{tool}");
        hand_over_a_file(&h, &user, conv, "codex").await?;
        fork_and_continue(&h, &user, conv).await?;
        compact_restore_cancel_and_expire(&h, &user, conv, "codex").await
    }
    .await;
    owner.kill().await?;
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires actual Claude SDK in the harness image"]
async fn a_claude_session_also_nests_subagents_and_asks_questions() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let mut owner = h.owner()?;
    let outcome = async {
        let conv = h
            .configured(&user, "work", json!({"harness":"claude"}), None)
            .await?;
        steer_then_configure(&h, &user, conv, "claude", "claude-opus-4-6").await?;
        let (run, _) = h.asking(&user, conv, "HARNESS_EXEC").await?;
        h.allow(&user, conv, run).await?;
        h.wait_state(conv, "completed").await?;
        h.idle(conv).await?;
        hand_over_a_file(&h, &user, conv, "claude").await?;
        // What a subagent does nests under the call that started it.
        h.submit(&user, conv, message("HARNESS_AGENT")).await?;
        let mut agent = None;
        for _ in 0..600 {
            let latest: Option<(Uuid, String)> = sqlx::query_as(
                "SELECT id,state FROM runs WHERE conversation_id=$1
                 ORDER BY started_at DESC LIMIT 1",
            )
            .bind(conv)
            .fetch_optional(&h.db)
            .await?;
            match latest {
                Some((run, state)) if state == "requires_action" => {
                    h.allow(&user, conv, run).await?
                }
                Some((run, state)) if state == "completed" => {
                    let delegated: bool = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM items
                           WHERE run_id=$1 AND text LIKE '%HARNESS_SUBAGENT%')",
                    )
                    .bind(run)
                    .fetch_one(&h.db)
                    .await?;
                    if delegated {
                        agent = Some(run);
                        break;
                    }
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let agent = agent.context("the subagent run did not complete")?;
        let nested: Vec<(String, String)> = sqlx::query_as(
            "SELECT c.kind,c.text FROM items c JOIN items p ON p.id=c.parent_item_id
             WHERE c.run_id=$1 AND p.text LIKE '%HARNESS_SUBAGENT%'",
        )
        .bind(agent)
        .fetch_all(&h.db)
        .await?;
        ensure!(
            nested
                .iter()
                .any(|(kind, text)| kind == "assistant" && text.contains("子任务完成")),
            "subagent items must nest under their call: {nested:?}"
        );
        h.idle(conv).await?;
        let (run, action) = h.asking(&user, conv, "HARNESS_QUESTION").await?;
        let (_, epoch) = h.action(run).await?;
        let answer = AnswerQuestion {
            action_id: action.to_string(),
            run_epoch: epoch as u64,
            answer: "Blue".into(),
        };
        h.submit(&user, conv, Command::AnswerQuestion(answer))
            .await?;
        h.wait_state(conv, "completed").await?;
        h.idle(conv).await?;
        fork_and_continue(&h, &user, conv).await?;
        compact_restore_cancel_and_expire(&h, &user, conv, "claude").await
    }
    .await;
    owner.kill().await?;
    h.close().await?;
    outcome
}

/// ACP has no forks.
#[tokio::test]
#[ignore = "requires actual OpenCode in the harness image"]
async fn an_opencode_session_over_acp_steers_approves_hands_over_restores_and_cancels()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let mut owner = h.owner()?;
    let outcome = async {
        let conv = h
            .configured(&user, "work", json!({"harness":"opencode"}), None)
            .await?;
        steer_then_configure(&h, &user, conv, "opencode", "claude-opus-4-6").await?;
        let (run, _) = h.asking(&user, conv, "HARNESS_EXEC").await?;
        h.allow(&user, conv, run).await?;
        h.wait_state(conv, "completed").await?;
        h.idle(conv).await?;
        hand_over_a_file(&h, &user, conv, "opencode").await?;
        compact_restore_cancel_and_expire(&h, &user, conv, "opencode").await
    }
    .await;
    owner.kill().await?;
    h.close().await?;
    outcome
}

/// The `<collaboration_mode>` of Codex's latest developer message.
fn collaboration_mode(body: &Value) -> String {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .filter_map(|content| content["text"].as_str())
        // The default instructions themselves quote this tag. Read the outer
        // opening tag, then select the most recent developer message.
        .filter_map(|text| text.split_once("<collaboration_mode>"))
        .next_back()
        .map(|(_, mode)| mode.to_owned())
        .unwrap_or_default()
}

#[tokio::test]
#[ignore = "requires actual Codex and Claude SDK in the harness image"]
async fn native_settings_reach_the_next_model_request() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let mut owner = h.owner()?;
    let outcome = async {
        for harness in ["claude", "codex"] {
            let settings = json!({"harness":harness,"plan":harness=="codex"});
            let conv = h.configured(&user, "work", settings, None).await?;
            h.turn(&user, conv, "initial settings").await?;
            let first = h.last_request();
            if harness == "claude" {
                ensure!(first["output_config"]["effort"] == "medium");
            } else {
                ensure!(collaboration_mode(&first).contains("# Plan Mode"));
            }
            for reasoning in ["high", "low"] {
                let update = UpdateRunConfig {
                    config_json: json!({"reasoning":reasoning,"plan":false}).to_string(),
                };
                h.submit(&user, conv, Command::UpdateRunConfig(update))
                    .await?;
                h.turn(&user, conv, "use the updated settings").await?;
                let request = h.last_request();
                if harness == "claude" {
                    ensure!(
                        request["output_config"]["effort"] == reasoning,
                        "Claude effort did not change: {}",
                        request["output_config"]
                    );
                } else {
                    ensure!(request["reasoning"]["effort"] == reasoning);
                    ensure!(
                        collaboration_mode(&request).contains("You are now in Default mode."),
                        "Codex did not exit Plan Mode: {:?}",
                        collaboration_mode(&request)
                    );
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    owner.kill().await?;
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires actual Codex in the harness image"]
async fn codex_forks_only_at_completed_turn_boundaries() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let mut owner = h.owner()?;
    let outcome = async {
        let conv = h
            .configured(&user, "work", json!({"harness":"codex"}), None)
            .await?;
        let (run, _) = h.asking(&user, conv, "HARNESS_FORK_BOUNDARY").await?;
        h.allow(&user, conv, run).await?;
        h.wait_state(conv, "completed").await?;
        h.idle(conv).await?;
        let reply = |text: &'static str| {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM items WHERE run_id=$1 AND kind='assistant' AND text=$2",
            )
            .bind(run)
            .bind(text)
            .fetch_one(&h.db)
        };
        let early = reply("HARNESS_BEFORE_TOOL").await?;
        let final_reply = reply("HARNESS_AFTER_TOOL").await?;
        let path = format!("/orgs/{{org}}/conversations/{conv}/commands");
        let catalog = h.json(&user, &path, Method::GET, json!({})).await?;
        let points = catalog["fork_points"].as_array().unwrap();
        ensure!(!points.contains(&json!(early)) && points.contains(&json!(final_reply)));
        let error = h.fork(&user, conv, early).await.unwrap_err();
        ensure!(
            error.to_string().contains("FORK_POINT_UNAVAILABLE"),
            "{error}"
        );
        h.turn(&user, conv, "HARNESS_FUTURE_CONTEXT").await?;
        let fork = h.fork(&user, conv, final_reply).await?;
        h.turn(&user, fork, "HARNESS_CHILD_CONTEXT").await?;
        let request = h.last_request().to_string();
        ensure!(
            request.contains("HARNESS_BEFORE_TOOL")
                && request.contains("HARNESS_AFTER_TOOL")
                && request.contains("HARNESS_TOOL_RESULT")
        );
        ensure!(
            !request.contains("HARNESS_FUTURE_CONTEXT"),
            "fork included a later turn"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    owner.kill().await?;
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires actual Claude SDK in the harness image"]
async fn a_claude_session_that_dies_mid_turn_fails_only_its_run() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let mut owner = h.owner()?;
    let outcome = async {
        let conv = h
            .configured(&user, "work", json!({"harness":"claude"}), None)
            .await?;
        let (run, _) = h.asking(&user, conv, "HARNESS_EXEC").await?;
        // Claude Code, the adapter's child, dies while the turn waits for an approval.
        let killed = h
            .exec(
                conv,
                "kill-claude",
                "pkill -9 -P \"$(pgrep -f 'dist/adapte[r]')\"",
            )
            .await?;
        ensure!(killed["exit_code"] == 0, "{killed}");
        h.wait_state(conv, "failed").await?;
        let error: Option<String> = sqlx::query_scalar("SELECT error_code FROM runs WHERE id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
        ensure!(
            error.as_deref() == Some("NATIVE_OUTCOME_UNKNOWN"),
            "{error:?}"
        );
        // The next run in the same sandbox gets a fresh harness.
        h.submit(&user, conv, message("hello again")).await?;
        h.wait_state(conv, "completed").await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    owner.kill().await?;
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires local PostgreSQL, NATS and containerd"]
async fn responses_chat_keeps_native_history_and_project_snapshots_until_the_project_is_purged()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let mut owner = h.owner()?;
    let outcome = async {
        let project = json!({"name":"Private knowledge","instructions":"PROJECT_SENTINEL",
            "memory_mode":"project_only",
            "defaults":{"mode":"chat","protocol":"responses","model":"gpt-5.4"}});
        let project = id(
            &h.json(&user, "/orgs/{org}/projects", Method::POST, project)
                .await?,
            "id",
        );
        let files = format!("/orgs/{{org}}/projects/{project}/files");
        let file = |content: &str| json!({"name":"knowledge.txt","content":content});
        h.json(&user, &files, Method::POST, file("Project answer: 42"))
            .await?;
        let other = h.user().await?;
        let denied = reqwest::Client::new()
            .get(format!("{}/orgs/{org}/projects/{project}/files", h.base))
            .bearer_auth(other["token"].as_str().unwrap())
            .send()
            .await?;
        ensure!(!denied.status().is_success());
        let immutable = reqwest::Client::new()
            .patch(format!("{}/orgs/{org}/projects/{project}", h.base))
            .bearer_auth(user["token"].as_str().unwrap())
            .json(&json!({"name":"bad","memory_mode":"default"}))
            .send()
            .await?;
        ensure!(immutable.status() == 400);
        let defaults = json!({"name":"Defaults",
            "defaults":{"mode":"work","harness":"claude","model":"claude-sonnet-4-6"}});
        let defaults = h
            .json(&user, "/orgs/{org}/projects", Method::POST, defaults)
            .await?;
        let ctx = h.context(&user).await?;
        let inherited = h
            .create(
                &ctx,
                "Inherit defaults",
                "",
                json!({}),
                Some(id(&defaults, "id")),
            )
            .await?;
        ensure!(inherited.mode == "work" && inherited.run_config["harness"] == "claude");
        let conv = h
            .configured(&user, "chat", json!({}), Some(project))
            .await?;
        let full_auto = h
            .configured(&user, "chat", json!({"approval":"full_auto"}), None)
            .await;
        ensure!(full_auto.unwrap_err().to_string().contains("POLICY_DENIED"));
        // Chat sandbox tools run without asking.
        h.submit(&user, conv, message("native-exec")).await?;
        let run = h.wait_state(conv, "completed").await?;
        let steps: Vec<Value> = sqlx::query_scalar(
            "SELECT native_response FROM loop_steps WHERE run_id=$1 ORDER BY step_no",
        )
        .bind(run)
        .fetch_all(&h.db)
        .await?;
        ensure!(
            steps.len() == 2 && steps[0]["output"][1]["encrypted_content"] == "opaque-reasoning"
        );
        {
            let requests = h.requests.lock().unwrap();
            let next = &requests
                .iter()
                .rfind(|(b, _)| b.get("input").is_some())
                .unwrap()
                .0;
            let input = next["input"].to_string();
            ensure!(input.contains("opaque-reasoning") && input.contains("function_call_output"));
            ensure!(
                next["instructions"]
                    .as_str()
                    .unwrap()
                    .contains("PROJECT_SENTINEL")
            );
        }
        let search = |run: Uuid, query: &'static str| {
            let app = &h.app;
            async move {
                let mut tx = app.store.tx(org).await?;
                let found = ryzhub_server::projects::search(&mut tx, org, run, query).await?;
                tx.commit().await?;
                anyhow::Ok(found)
            }
        };
        ensure!(search(run, "42").await?["matches"][0]["file"] == "knowledge.txt");
        h.json(&user, &files, Method::POST, file("Project answer: 84"))
            .await?;
        ensure!(
            !search(run, "42").await?["matches"]
                .as_array()
                .unwrap()
                .is_empty(),
            "a run keeps its project snapshot"
        );
        h.submit(&user, conv, Command::Compact(Compact {})).await?;
        let compact = h.wait_state(conv, "completed").await?;
        let response: Value =
            sqlx::query_scalar("SELECT native_response FROM loop_steps WHERE run_id=$1")
                .bind(compact)
                .fetch_one(&h.db)
                .await?;
        ensure!(response["output"][0]["type"] == "compaction");
        h.submit(&user, conv, message("after compact")).await?;
        let run = h.wait_state(conv, "completed").await?;
        ensure!(
            !search(run, "84").await?["matches"]
                .as_array()
                .unwrap()
                .is_empty(),
            "the next run uses edited files"
        );
        let request: Value = sqlx::query_scalar(
            "SELECT request_messages FROM loop_steps WHERE run_id=$1 AND step_no=0",
        )
        .bind(run)
        .fetch_one(&h.db)
        .await?;
        ensure!(request.to_string().contains("opaque-compaction"));
        let item: Uuid = sqlx::query_scalar("SELECT assistant_item_id FROM runs WHERE id=$1")
            .bind(run)
            .fetch_one(&h.db)
            .await?;
        let fork = h.fork(&user, conv, item).await?;
        h.submit(&user, fork, message("forked native continuation"))
            .await?;
        h.wait_state(fork, "completed").await?;
        // Moved out, a conversation outlives its project; the fork, still in it, does not.
        let moved = format!("/orgs/{{org}}/conversations/{conv}/project");
        h.json(&user, &moved, Method::POST, json!({"project_id":null}))
            .await?;
        let path = format!("/orgs/{{org}}/projects/{project}");
        h.json(&user, &path, Method::DELETE, json!({})).await?;
        let state = |conv: Uuid| {
            sqlx::query_scalar::<_, String>("SELECT state FROM conversations WHERE id=$1")
                .bind(conv)
                .fetch_one(&h.db)
        };
        ensure!(state(fork).await? == "deletion_pending");
        let restore = format!("/orgs/{{org}}/projects/{project}/restore");
        h.json(&user, &restore, Method::POST, json!({})).await?;
        h.json(&user, &path, Method::DELETE, json!({})).await?;
        let collect = || ryzhub_server::projects::collect(&h.app, org, project);
        ensure!(!collect().await?, "a recoverable project cannot be purged");
        sqlx::query("UPDATE projects SET deleted_at=now()-interval '31 days' WHERE id=$1")
            .bind(project)
            .execute(&h.db)
            .await?;
        ensure!(collect().await?);
        ensure!(!collect().await?, "collection is idempotent");
        let remains: i64 =
            sqlx::query_scalar("SELECT count(*) FROM project_files WHERE project_id=$1")
                .bind(project)
                .fetch_one(&h.db)
                .await?;
        ensure!(remains == 0);
        let erased: bool = sqlx::query_scalar(
            "SELECT state='deleted' AND context_seed IS NULL FROM conversations WHERE id=$1",
        )
        .bind(fork)
        .fetch_one(&h.db)
        .await?;
        ensure!(erased);
        ensure!(
            state(conv).await? == "active",
            "the moved-out conversation survives"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    owner.kill().await?;
    h.close().await?;
    outcome
}
