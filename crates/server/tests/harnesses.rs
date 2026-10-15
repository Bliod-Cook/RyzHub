//! Real PostgreSQL, NATS and containerd; only the paid model upstream is a protocol fixture.
mod common;
use anyhow::{Context, ensure};
use common::{Harness, guest_exec, id, message};
use ryzhub_proto::{command_envelope::Command, *};
use ryzhub_server::config::ModelRoute;
use serde_json::{Value, json};
use sqlx::PgPool;
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
        "provider": {"anthropic": {"options": {"baseURL": "{proxy}/v1", "apiKey": "guest-proxy"}, "models": "{models}"}},
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
            config.llm.responses_model = "gpt-5.4".into();
            config.llm.models = vec![
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
            .app
            .store
            .create_configured(
                &ctx,
                "Harness",
                mode,
                settings,
                project,
                Some(&self.app.config),
            )
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
 // Chat sandbox tools run without asking.
 h.submit(&user,conv,message("native-exec")).await?;let run=h.wait_state(conv,"completed").await?;
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
#[ignore = "requires actual Codex and Claude SDK in the harness image"]
async fn two_harnesses_steer_commands_fork_and_restore() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let mut owner = h.owner()?;
    let outcome=async {
 for harness in ["codex","claude","opencode"] {
  println!("harness: {harness}");
  let conv=h.configured(&user,"work",json!({"harness":harness}),None).await?;
  h.submit(&user,conv,message("slow hello")).await?;let run=h.wait_state(conv,"running").await?;
  // Steer immediately, including while the native session is still being provisioned.
  h.submit(&user,conv,Command::Steer(Steer{text:"second queued instruction".into(),..Default::default()})).await?;
  let model=if harness=="codex" {"gpt-5.2"}else{"claude-opus-4-6"};
  h.submit(&user,conv,Command::UpdateRunConfig(UpdateRunConfig{config_json:json!({"model":model,"reasoning":"high","plan":true}).to_string()})).await?;
  let snapshot:Value=sqlx::query_scalar("SELECT run_config FROM runs WHERE id=$1").bind(run).fetch_one(&h.db).await?;ensure!(snapshot["model"]!=model && snapshot["plan"]==false,"settings must not alter the active run");
  h.wait_state(conv,"completed").await?;
  let steer:String=sqlx::query_scalar("SELECT state FROM items WHERE run_id=$1 AND kind='steer'").bind(run).fetch_one(&h.db).await?;ensure!(steer=="handed","{harness} steer: {steer}");
  if harness!="codex" {let turns:i64=sqlx::query_scalar("SELECT count(*) FROM native_turns WHERE run_id=$1").bind(run).fetch_one(&h.db).await?;ensure!(turns==2,"{harness} must queue into a second native turn: {turns}");}
  h.idle(conv).await?;
  h.submit(&user,conv,message("plan a change")).await?;let configured=h.wait_state(conv,"completed").await?;
  let settings:Value=sqlx::query_scalar("SELECT run_config FROM runs WHERE id=$1").bind(configured).fetch_one(&h.db).await?;ensure!(settings["plan"]==true && settings["reasoning"]=="high" && settings["model"]==model);
  ensure!(h.requests.lock().unwrap().iter().any(|(b,_)|b["model"]==model),"native model switch must reach upstream");
  h.idle(conv).await?;
  h.submit(&user,conv,Command::UpdateRunConfig(UpdateRunConfig{config_json:json!({"plan":false}).to_string()})).await?;
  h.submit(&user,conv,message(if harness=="codex"{"native-exec"}else{"HARNESS_EXEC"})).await?;let run=h.wait_state(conv,"requires_action").await?;let (action,epoch)=h.action(run).await?;
  h.submit(&user,conv,Command::DecideApproval(DecideApproval{action_id:action.to_string(),run_epoch:epoch as u64,decision:"allow".into()})).await?;h.wait_state(conv,"completed").await?;h.idle(conv).await?;
  // A reply hands over the workspace files it links to.
  let (sandbox,epoch):(Uuid,i64)=sqlx::query_as("SELECT id,exec_epoch FROM sandboxes WHERE conversation_id=$1 AND state='running'").bind(conv).fetch_one(&h.db).await?;
  guest_exec(std::path::Path::new(&h.app.config.sandbox.host_socket),sandbox,epoch,"share-report","mkdir -p /workspace/share && printf 'work report' > /workspace/share/report.txt").await?;
  h.submit(&user,conv,message("HARNESS_SHARE")).await?;let shared=h.wait_state(conv,"completed").await?;h.idle(conv).await?;
  let detail:Value=sqlx::query_scalar("SELECT detail FROM items WHERE run_id=$1 AND kind='assistant' AND state='completed' AND text LIKE '%sandbox:%'").bind(shared).fetch_one(&h.db).await?;
  let file=&detail["kind"]["Answer"]["files"][0];
  ensure!(file["link"]=="/workspace/share/report.txt" && file["file"]["name"]=="report.txt" && file["file"]["size"]==11,"{harness} hand-over: {detail}");
  if harness=="claude" {
   // What a subagent does nests under the call that started it.
   h.submit(&user,conv,message("HARNESS_AGENT")).await?;
   let mut agent=None;
   for _ in 0..600 {
    let latest:Option<(Uuid,String)>=sqlx::query_as("SELECT id,state FROM runs WHERE conversation_id=$1 ORDER BY started_at DESC LIMIT 1").bind(conv).fetch_optional(&h.db).await?;
    match latest {
     Some((run,state)) if state=="requires_action" => {let (action,epoch)=h.action(run).await?;h.submit(&user,conv,Command::DecideApproval(DecideApproval{action_id:action.to_string(),run_epoch:epoch as u64,decision:"allow".into()})).await?;}
     Some((run,state)) if state=="completed" && sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM items WHERE run_id=$1 AND text LIKE '%HARNESS_SUBAGENT%')").bind(run).fetch_one(&h.db).await? => {agent=Some(run);break;}
     _ => {}
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
   }
   let agent=agent.context("the subagent run did not complete")?;
   let nested:Vec<(String,String)>=sqlx::query_as("SELECT c.kind,c.text FROM items c JOIN items p ON p.id=c.parent_item_id WHERE c.run_id=$1 AND p.text LIKE '%HARNESS_SUBAGENT%'").bind(agent).fetch_all(&h.db).await?;
   let all:Vec<(String,String,Option<Uuid>)>=sqlx::query_as("SELECT kind,left(text,80),parent_item_id FROM items WHERE run_id=$1 ORDER BY first_seq").bind(agent).fetch_all(&h.db).await?;
   ensure!(nested.iter().any(|(kind,text)|kind=="assistant" && text.contains("子任务完成")),"subagent items must nest under their call: {nested:?} in {all:?}");
   h.idle(conv).await?;
   h.submit(&user,conv,message("HARNESS_QUESTION")).await?;let run=h.wait_state(conv,"requires_action").await?;let (action,epoch)=h.action(run).await?;
   h.submit(&user,conv,Command::AnswerQuestion(AnswerQuestion{action_id:action.to_string(),run_epoch:epoch as u64,answer:"Blue".into()})).await?;h.wait_state(conv,"completed").await?;h.idle(conv).await?;
  }
  // ACP has no forks.
  if harness!="opencode" {
  let latest:Uuid=sqlx::query_scalar("SELECT id FROM items WHERE conversation_id=$1 AND kind='assistant' AND state='completed' ORDER BY last_seq DESC LIMIT 1").bind(conv).fetch_one(&h.db).await?;
  let fork=h.submit(&user,conv,Command::Fork(Fork{item_id:latest.to_string()})).await?;let fork=h.fork_result(fork).await?;
  h.submit(&user,fork,message("hello fork")).await?;h.wait_state(fork,"completed").await?;h.idle(fork).await?;
  }
  h.submit(&user,conv,Command::Compact(Compact{})).await?;h.wait_state(conv,"completed").await?;h.idle(conv).await?;
  let job=h.json(&user,&format!("/orgs/{org}/conversations/{conv}/workspace"),reqwest::Method::POST,json!({"kind":"restore"})).await?;let job=id(&job,"id");
  for _ in 0..1000 {let state:String=sqlx::query_scalar("SELECT state FROM workspace_jobs WHERE id=$1").bind(job).fetch_one(&h.db).await?;if state=="completed"{break;}else if state=="failed"{let result:Option<Value>=sqlx::query_scalar("SELECT result FROM workspace_jobs WHERE id=$1").bind(job).fetch_one(&h.db).await?;anyhow::bail!("{harness} restore failed: {result:?}");}tokio::time::sleep(Duration::from_millis(100)).await;}
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

#[path = "harnesses/regressions.rs"]
mod regressions;
