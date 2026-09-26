//! Native Work adapter. Owner leases fence projection writes; the guest owns the harness.
use crate::{
    api::App,
    chat::{self, Progress, commands, committed_item},
    sandbox,
    store::{ActionRow, Lease, Tx, append},
};
use anyhow::{Context, ensure};
use ryzhub_execution::{GuestCommand, GuestEvent, HostCommand, SandboxSpec};
use ryzhub_proto::{Item, RunState, event_envelope::Event};
use serde_json::{Value, json};
use sqlx::Row;
use tokio::sync::mpsc;
use uuid::Uuid;

pub fn answer(params: &Value, kind: &str, text: &str) -> anyhow::Result<Value> {
    if kind != "question" {
        return Ok(json!({"decision":if text=="allow"{"accept"}else{"decline"}}));
    }
    let questions = params["questions"]
        .as_array()
        .context("INVALID_QUESTIONS")?;
    let supplied: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let mut answers = serde_json::Map::new();
    for q in questions {
        let id = q["id"].as_str().context("INVALID_QUESTION_ID")?;
        let answer = if questions.len() == 1 {
            text
        } else {
            supplied[id]
                .as_str()
                .context("ANSWER_REQUIRES_JSON_QUESTION_IDS")?
        };
        answers.insert(id.into(), json!({"answers":[answer]}));
    }
    Ok(json!({"answers":answers}))
}
pub(crate) async fn rpc(
    app: &App,
    lease: &Lease,
    id: Uuid,
    op: String,
    method: &str,
    params: Value,
) -> anyhow::Result<Value> {
    let mut result = sandbox::guest(
        app,
        lease,
        id,
        GuestCommand::NativeRequest {
            op_id: op.clone(),
            method: method.into(),
            params,
        },
    )
    .await?;
    for _ in 0..300 {
        if result["state"] != "running" {
            ensure!(result["state"] == "completed", "NATIVE_OUTCOME_UNKNOWN");
            let response = &result["response"];
            ensure!(
                response.get("error").is_none(),
                "NATIVE_RPC_ERROR: {}",
                response["error"]
            );
            return Ok(response["result"].clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        result = sandbox::guest(app, lease, id, GuestCommand::Poll { op_id: op.clone() }).await?;
    }
    anyhow::bail!("NATIVE_RPC_TIMEOUT")
}
pub async fn ensure_work(app: &App, lease: &Lease, run: Uuid) -> anyhow::Result<Uuid> {
    let settings = run_settings(app, lease, run).await?;
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    let old:Option<(Uuid,Uuid,i64)>=sqlx::query_as("SELECT id,run_id,lease_epoch FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent AND state IN ('running','provisioning') FOR UPDATE").bind(lease.org_id).bind(lease.conversation_id).fetch_optional(&mut *tx).await?;
    available(&mut tx, lease).await?;
    let fork: Option<Value> =
        sqlx::query_scalar("SELECT native_fork FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut *tx)
            .await?;
    if old.is_none() && fork.is_none() {
        let used: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent) OR EXISTS(SELECT 1 FROM workspace_heads WHERE org_id=$1 AND conversation_id=$2 AND checkpoint_id IS NOT NULL)")
            .bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut *tx).await?;
        ensure!(!used, "WORKSPACE_RECOVERY_REQUIRED");
    }
    if let Some((id, _, _)) = old {
        let valid:bool=sqlx::query_scalar("SELECT node_lease_until IS NULL OR node_lease_until>clock_timestamp() FROM sandboxes WHERE org_id=$1 AND id=$2").bind(lease.org_id).bind(id).fetch_one(&mut *tx).await?;
        ensure!(valid, "WORKSPACE_RECOVERY_REQUIRED");
    }
    let (id, previous, generation) = old.unwrap_or_else(|| (Uuid::now_v7(), run, 1));
    let cfg = &app.config.sandbox;
    let mut spec = SandboxSpec {
        id,
        org_id: lease.org_id,
        workspace_id: lease.conversation_id,
        exec_epoch: lease.epoch as u64,
        image: cfg.image.clone(),
        runtime: cfg.runtime.clone(),
        memory_bytes: cfg.memory_bytes.max(512 * 1024 * 1024),
        pids_limit: cfg.pids_limit,
        persistent: true,
    };
    if old.is_some() {
        let handle: Value =
            sqlx::query_scalar("SELECT handle FROM sandboxes WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if let Ok(pinned) = serde_json::from_value::<SandboxSpec>(handle) {
            spec.image = pinned.image;
            spec.runtime = pinned.runtime;
        }
    } else if let Some(fork) = fork.as_ref() {
        spec.image = fork["manifest"]["image"]
            .as_str()
            .context("INVALID_MANIFEST")?
            .into();
    }
    if old.is_none() {
        sqlx::query("INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent) VALUES($1,$2,$3,$4,$5,$6,'provisioning',$7,true)").bind(id).bind(lease.org_id).bind(lease.conversation_id).bind(run).bind(generation).bind(lease.epoch).bind(serde_json::to_value(&spec)?).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    if let Some(fork) = fork.as_ref() {
        crate::checkpoint::restore_fork(app, lease, spec, &fork["manifest"]).await?;
    } else {
        sandbox::host(app, HostCommand::Create { spec }).await?;
    }
    sandbox::guest(app, lease, id, GuestCommand::Fence).await?;
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    sqlx::query("UPDATE sandboxes SET state='running',exec_epoch=$3,run_id=$4,node_lease_until=now()+interval '15 seconds' WHERE org_id=$1 AND id=$2").bind(lease.org_id).bind(id).bind(lease.epoch).bind(run).execute(&mut *tx).await?;
    sqlx::query("UPDATE runs SET sandbox_id=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(run)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    refresh_key(app, lease, run, id).await?;
    crate::projects::mount(app, lease, run, id).await?;
    sandbox::guest(
        app,
        lease,
        id,
        GuestCommand::NativeStart {
            harness: settings.harness.clone(),
            model: settings.model.clone(),
            mcp: !app.config.work.mcp_tools.is_empty(),
        },
    )
    .await?;
    let models = rpc(
        app,
        lease,
        id,
        format!("{id}:models"),
        "model/list",
        json!({}),
    )
    .await?;
    let mut caps = crate::run_config::capabilities(&settings.harness, &settings.protocol);
    caps["models"] = models["data"].clone();
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    sqlx::query("UPDATE sandboxes SET capabilities=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(id)
        .bind(caps)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let _ = previous;
    Ok(id)
}

/// A failed restore is a durable workspace state, not permission to start from scratch.
pub async fn available(tx: &mut Tx<'_>, lease: &Lease) -> anyhow::Result<()> {
    let recovery: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_heads WHERE org_id=$1 AND conversation_id=$2 AND recovery_required)")
        .bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut **tx).await?;
    ensure!(!recovery, "WORKSPACE_RECOVERY_REQUIRED");
    Ok(())
}

pub async fn refresh_key(app: &App, lease: &Lease, run: Uuid, id: Uuid) -> anyhow::Result<()> {
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    let generation: i64 = sqlx::query_scalar("SELECT lease_epoch FROM sandboxes WHERE org_id=$1 AND id=$2 AND run_id=$3 AND state='running'")
        .bind(lease.org_id).bind(id).bind(run).fetch_one(&mut *tx).await?;
    let user: Uuid =
        sqlx::query_scalar("SELECT creator_id FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut *tx)
            .await?;
    let credential: Option<Uuid> =
        sqlx::query_scalar("SELECT credential_id FROM runs WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(run)
            .fetch_one(&mut *tx)
            .await?;
    // Reconstruct a still-live signed token: no vault token is stored in the filesystem.
    let existing:Option<(Uuid,i64)>=sqlx::query_as("SELECT id,extract(epoch FROM expires_at)::bigint FROM virtual_keys WHERE org_id=$1 AND sandbox_id=$2 AND run_id=$3 AND revoked_at IS NULL AND superseded_at IS NULL AND expires_at>now()+interval '5 minutes' ORDER BY expires_at DESC LIMIT 1").bind(lease.org_id).bind(id).bind(run).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    let mut claims = crate::llm::identity::loop_identity(lease, user, run, &app.config.llm);
    claims.kind = "sandbox".into();
    claims.exec_epoch = None;
    claims.sandbox_id = Some(id);
    claims.lease_epoch = Some(generation);
    claims.credential_id = credential;
    {
        let mut tx = app.store.tx(lease.org_id).await?;
        let model: String = sqlx::query_scalar("SELECT model FROM runs WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(run)
            .fetch_one(&mut *tx)
            .await?;
        claims.models = vec![model];
        tx.commit().await?;
    }
    let token = if let Some((jti, exp)) = existing {
        claims.jti = jti;
        claims.exp = exp;
        crate::llm::identity::sign(&app.config.llm, &claims)?
    } else {
        crate::llm::gateway::issue_sandbox_key(app, &mut claims).await?
    };
    let socket_name = std::path::Path::new(
        app.config
            .llm
            .unix_socket
            .as_ref()
            .context("WORK_REQUIRES_GATEWAY_SOCKET")?,
    )
    .file_name()
    .context("INVALID_SOCKET")?
    .to_string_lossy()
    .into_owned();
    sandbox::guest(
        app,
        lease,
        id,
        GuestCommand::RotateKey { token, socket_name },
    )
    .await?;
    Ok(())
}
pub async fn drive(
    app: App,
    lease: Lease,
    run: Uuid,
    sender: mpsc::Sender<Progress>,
) -> anyhow::Result<()> {
    let id = ensure_work(&app, &lease, run).await?;
    let settings = run_settings(&app, &lease, run).await?;
    let mut tx = app.store.tx(lease.org_id).await?;
    let thread: Option<String> =
        sqlx::query_scalar("SELECT native_thread_id FROM sandboxes WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let recovered: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND id=$2 AND kind='restore')",
    )
    .bind(lease.org_id)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let fork: Option<Value> =
        sqlx::query_scalar("SELECT native_fork FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    let mut tx = app.store.tx(lease.org_id).await?;
    let project_instructions = crate::projects::instructions(&mut tx, lease.org_id, run).await?;
    tx.commit().await?;
    let mut session_params = json!({"model":settings.model,"modelProvider":"ryzhub","cwd":"/workspace","approvalPolicy":if settings.approval=="full_auto"{"never"}else{"untrusted"},"sandbox":"danger-full-access","persistExtendedHistory":true,"config":settings});
    session_params["developerInstructions"] = json!(project_instructions);
    if settings.harness == "codex" {
        session_params.as_object_mut().unwrap().remove("config");
    }
    let thread = if let Some(fork) = fork {
        session_params["threadId"] = fork["manifest"]["thread_id"].clone();
        session_params["lastTurnId"] = fork["last_turn_id"].clone();
        session_params["resumeSessionAt"] = fork["message_id"].clone();
        let result = rpc(
            &app,
            &lease,
            id,
            format!("{id}:fork"),
            "thread/fork",
            session_params.clone(),
        )
        .await?;
        let thread = result["thread"]["id"]
            .as_str()
            .context("NATIVE_THREAD_MISSING")?
            .to_owned();
        let mut tx = app.store.tx(lease.org_id).await?;
        chat::lock(&mut tx, &lease).await?;
        sqlx::query("UPDATE sandboxes SET native_thread_id=$3 WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(id)
            .bind(&thread)
            .execute(&mut *tx)
            .await?;
        if let Some(checkpoint) = fork["checkpoint_id"].as_str() {
            sqlx::query("DELETE FROM restore_pins WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(Uuid::parse_str(checkpoint)?)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE conversations SET native_fork=NULL WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        thread
    } else if let Some(thread) = thread {
        session_params["threadId"] = json!(thread);
        rpc(
            &app,
            &lease,
            id,
            format!("{run}:resume"),
            "thread/resume",
            session_params.clone(),
        )
        .await?;
        thread
    } else {
        let result = rpc(
            &app,
            &lease,
            id,
            format!("{id}:thread"),
            "thread/start",
            session_params.clone(),
        )
        .await?;
        let thread = result["thread"]["id"]
            .as_str()
            .context("NATIVE_THREAD_MISSING")?
            .to_owned();
        let mut tx = app.store.tx(lease.org_id).await?;
        chat::lock(&mut tx, &lease).await?;
        sqlx::query("UPDATE sandboxes SET native_thread_id=$3 WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(id)
            .bind(&thread)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        thread
    };
    if settings.harness == "claude" {
        rpc(
            &app,
            &lease,
            id,
            format!("{run}:config"),
            "session/configure",
            {
                let mut config = serde_json::to_value(&settings)?;
                config["project"] = json!(project_instructions);
                config
            },
        )
        .await?;
    }
    let mut renewed = tokio::time::Instant::now();
    loop {
        if renewed.elapsed() >= std::time::Duration::from_secs(60) {
            refresh_key(&app, &lease, run, id).await?;
            renewed = tokio::time::Instant::now();
        }
        crate::mcp::project(&app, &lease, run).await?;
        let mut tx = app.store.tx(lease.org_id).await?;
        chat::lock(&mut tx, &lease).await?;
        let row = sqlx::query(
            "SELECT state,native_turn_id,operation FROM runs WHERE org_id=$1 AND id=$2",
        )
        .bind(lease.org_id)
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
        let state: String = row.get("state");
        let turn: Option<String> = row.get("native_turn_id");
        let operation: String = row.get("operation");
        if !matches!(state.as_str(), "running" | "requires_action") {
            tx.commit().await?;
            if let Some(turn) = turn {
                let _ = rpc(
                    &app,
                    &lease,
                    id,
                    format!("{run}:interrupt"),
                    "turn/interrupt",
                    json!({"threadId":thread,"turnId":turn}),
                )
                .await;
            }
            return Ok(());
        }
        let inputs:Vec<(i64,String,String)>=sqlx::query_as("SELECT id,text,kind FROM pending_inputs WHERE org_id=$1 AND run_id=$2 AND consumed_step IS NULL ORDER BY id").bind(lease.org_id).bind(run).fetch_all(&mut *tx).await?;
        let answers:Vec<(String,Value)>=sqlx::query_as("SELECT request_id,answer FROM native_requests WHERE org_id=$1 AND sandbox_id=$2 AND answer IS NOT NULL AND NOT delivered").bind(lease.org_id).bind(id).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        for (request, result) in answers {
            sandbox::guest(
                &app,
                &lease,
                id,
                GuestCommand::NativeAnswer {
                    op_id: format!("answer:{request}"),
                    id: serde_json::from_str(&request)?,
                    result,
                },
            )
            .await?;
            let mut tx = app.store.tx(lease.org_id).await?;
            chat::lock(&mut tx, &lease).await?;
            sqlx::query("UPDATE native_requests SET delivered=true WHERE org_id=$1 AND sandbox_id=$2 AND request_id=$3").bind(lease.org_id).bind(id).bind(request).execute(&mut *tx).await?;
            tx.commit().await?;
        }
        for (seq, text, kind) in inputs {
            let queued = kind == "steer" && settings.harness == "claude";
            if kind == "steer" && ((queued && turn.is_some()) || (!queued && turn.is_none())) {
                let mut tx = app.store.tx(lease.org_id).await?;
                chat::lock(&mut tx, &lease).await?;
                let item_id: Uuid = sqlx::query_scalar(
                    "SELECT item_id FROM pending_inputs WHERE org_id=$1 AND id=$2",
                )
                .bind(lease.org_id)
                .bind(seq)
                .fetch_one(&mut *tx)
                .await?;
                let mut item = commands::item(&mut tx, lease.org_id, item_id).await?;
                if item.state != "waiting" {
                    item.revision += 1;
                    item.state = "waiting".into();
                    committed_item(&mut tx, &lease, item).await?;
                }
                tx.commit().await?;
                continue;
            }
            let (method, mut params) = if kind == "message" || queued {
                let text = if recovered {
                    format!(
                        "[平台恢复说明：工作区与原生会话已恢复到已提交检查点。水位之后的操作已标记失效，不应假定它们仍然生效。]\n{text}"
                    )
                } else {
                    text
                };
                (
                    "turn/start",
                    json!({"threadId":thread,"input":[{"type":"text","text":text,"text_elements":[]}]}),
                )
            } else {
                (
                    "turn/steer",
                    json!({"threadId":thread,"expectedTurnId":turn,"input":[{"type":"text","text":text,"text_elements":[]}]}),
                )
            };
            if method == "turn/start" && settings.harness == "codex" {
                params["model"] = json!(settings.model);
                params["effort"] = json!(settings.reasoning);
                params["approvalPolicy"] = json!(if settings.approval == "full_auto" {
                    "never"
                } else {
                    "untrusted"
                });
                // Omitting this field preserves the previous turn's mode.
                params["collaborationMode"] = json!({"mode":if settings.plan {"plan"} else {"default"},"settings":{"model":settings.model,"reasoning_effort":settings.reasoning,"developer_instructions":null}});
            }
            let method = if operation == "compact" && settings.harness == "codex" {
                "thread/compact/start"
            } else {
                method
            };
            let result = rpc(
                &app,
                &lease,
                id,
                format!("{run}:input:{seq}"),
                method,
                params,
            )
            .await?;
            let mut tx = app.store.tx(lease.org_id).await?;
            chat::lock(&mut tx, &lease).await?;
            if (kind == "message" || queued) && method != "thread/compact/start" {
                sqlx::query("UPDATE runs SET native_turn_id=$3 WHERE org_id=$1 AND id=$2")
                    .bind(lease.org_id)
                    .bind(run)
                    .bind(
                        result["turn"]["id"]
                            .as_str()
                            .context("NATIVE_TURN_MISSING")?,
                    )
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO native_turns(org_id,sandbox_id,turn_id,run_id) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING").bind(lease.org_id).bind(id).bind(result["turn"]["id"].as_str()).bind(run).execute(&mut *tx).await?;
            }
            let item_id:Uuid=sqlx::query_scalar("UPDATE pending_inputs SET consumed_step=0 WHERE org_id=$1 AND id=$2 RETURNING item_id").bind(lease.org_id).bind(seq).fetch_one(&mut *tx).await?;
            if kind == "steer" {
                let mut item = commands::item(&mut tx, lease.org_id, item_id).await?;
                item.revision += 1;
                item.state = "handed".into();
                committed_item(&mut tx, &lease, item).await?;
            }
            tx.commit().await?;
            // A newly registered turn changes how subsequent steer inputs are delivered.
            // Reload it before processing another input from this batch.
            if kind == "message" || queued {
                break;
            }
        }
        if sync(&app, &lease, run, id, &sender, None).await? {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

pub async fn sync(
    app: &App,
    lease: &Lease,
    run: Uuid,
    id: Uuid,
    sender: &mpsc::Sender<Progress>,
    through: Option<u64>,
) -> anyhow::Result<bool> {
    let mut tx = app.store.tx(lease.org_id).await?;
    let after: i64 =
        sqlx::query_scalar("SELECT acked_gseq FROM sandboxes WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    let replay = sandbox::guest(
        app,
        lease,
        id,
        GuestCommand::Replay {
            after: after as u64,
        },
    )
    .await?;
    let events: Vec<GuestEvent> = serde_json::from_value(replay["events"].clone())?;
    let mut tx = app.store.tx(lease.org_id).await?;
    chat::lock(&mut tx, lease).await?;
    let mut current: i64 =
        sqlx::query_scalar("SELECT acked_gseq FROM sandboxes WHERE org_id=$1 AND id=$2 FOR UPDATE")
            .bind(lease.org_id)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let mut done = false;
    for event in events {
        if event.gseq <= current as u64 {
            continue;
        }
        if through.is_some_and(|t| event.gseq > t) {
            break;
        }
        ensure!(event.gseq == current as u64 + 1, "GUEST_EVENT_GAP");
        // A checkpoint can overlap a turn transition. Route by the native turn,
        // never by the run that happened to initiate this replay.
        let mut event_run = run;
        let method = event.result["method"].as_str().unwrap_or("");
        // Resume emits historical usage/status notifications; those have no run projection.
        if method == "turn/started" {
            let turn = event.result["params"]["turn"]["id"].as_str();
            sqlx::query("UPDATE runs SET native_turn_id=$3 WHERE org_id=$1 AND id=$2 AND operation='compact' AND native_turn_id IS NULL AND state='running'").bind(lease.org_id).bind(run).bind(turn).execute(&mut *tx).await?;
        }
        let projects_run = matches!(method, "item/started" | "item/completed" | "turn/completed")
            || (event.result.get("id").is_some() && !method.is_empty());
        if event.op_id == "native"
            && projects_run
            && let Some(turn) = event.result["params"]["turnId"]
                .as_str()
                .or_else(|| event.result["params"]["turn"]["id"].as_str())
        {
            let target: Option<Uuid> = sqlx::query_scalar("SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2 AND sandbox_id=$3 AND (native_turn_id=$4 OR EXISTS(SELECT 1 FROM native_turns nt WHERE nt.org_id=runs.org_id AND nt.sandbox_id=$3 AND nt.turn_id=$4 AND nt.run_id=runs.id))")
                .bind(lease.org_id).bind(lease.conversation_id).bind(id).bind(turn)
                .fetch_optional(&mut *tx).await?;
            let Some(target) = target else {
                // turn/start may have replied to the guest before its id is committed.
                // Leave this event unacknowledged until its owning run is registered.
                break;
            };
            event_run = target;
        }
        sqlx::query("INSERT INTO guest_events(org_id,sandbox_id,gseq,payload) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING").bind(lease.org_id).bind(id).bind(event.gseq as i64).bind(serde_json::to_value(&event)?).execute(&mut *tx).await?;
        if event.op_id == "native" {
            let finished = project(&mut tx, lease, event_run, id, &event.result).await?;
            done |= event_run == run && finished;
        }
        let seq: i64 =
            sqlx::query_scalar("SELECT last_seq FROM channels WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(lease.channel())
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query(
            "UPDATE guest_events SET channel_seq=$4 WHERE org_id=$1 AND sandbox_id=$2 AND gseq=$3",
        )
        .bind(lease.org_id)
        .bind(id)
        .bind(event.gseq as i64)
        .bind(seq)
        .execute(&mut *tx)
        .await?;
        current = event.gseq as i64;
    }
    sqlx::query("UPDATE sandboxes SET acked_gseq=$3,node_lease_until=now()+interval '15 seconds' WHERE org_id=$1 AND id=$2").bind(lease.org_id).bind(id).bind(current).execute(&mut *tx).await?;
    let mut progress = vec![];
    if !done
        && through.is_none()
        && let Some(folds) = replay["folds"].as_object()
    {
        for (native, fold) in folds {
            let item:Option<Uuid>=sqlx::query_scalar("SELECT item_id FROM native_items WHERE org_id=$1 AND sandbox_id=$2 AND native_id=$3").bind(lease.org_id).bind(id).bind(native).fetch_optional(&mut *tx).await?;
            if let Some(item) = item {
                let item = commands::item(&mut tx, lease.org_id, item).await?;
                if item.state == "streaming" && fold["method"] == "item/agentMessage/delta" {
                    progress.push(Progress {
                        item,
                        text: fold["text"].as_str().unwrap_or("").into(),
                        offset: 0,
                    });
                }
            }
        }
    }
    tx.commit().await?;
    sandbox::guest(
        app,
        lease,
        id,
        GuestCommand::Ack {
            through: current as u64,
        },
    )
    .await?;
    for p in progress {
        let _ = sender.send(p).await;
    }
    Ok(done)
}
async fn project(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    sandbox: Uuid,
    event: &Value,
) -> anyhow::Result<bool> {
    let method = event["method"].as_str().unwrap_or("");
    let p = &event["params"];
    let event_turn = p["turnId"].as_str().or_else(|| p["turn"]["id"].as_str());
    if let Some(event_turn) = event_turn {
        let current: Option<String> =
            sqlx::query_scalar("SELECT native_turn_id FROM runs WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(run)
                .fetch_one(&mut **tx)
                .await?;
        if current
            .as_deref()
            .is_some_and(|current| current != event_turn)
        {
            let known:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_turns WHERE org_id=$1 AND sandbox_id=$2 AND turn_id=$3 AND run_id=$4)").bind(lease.org_id).bind(sandbox).bind(event_turn).bind(run).fetch_one(&mut **tx).await?;
            if !known {
                return Ok(false);
            }
        }
    }
    if method == "session/capabilities" {
        sqlx::query("UPDATE sandboxes SET commands=$3,capabilities=jsonb_set(capabilities,'{models}',$4) WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(sandbox)
            .bind(&p["commands"])
            .bind(&p["models"])
            .execute(&mut **tx)
            .await?;
    }
    if event.get("id").is_some() && !method.is_empty() {
        if matches!(
            method,
            "item/commandExecution/requestApproval"
                | "item/fileChange/requestApproval"
                | "item/tool/requestUserInput"
        ) {
            let settings: Value =
                sqlx::query_scalar("SELECT run_config FROM runs WHERE org_id=$1 AND id=$2")
                    .bind(lease.org_id)
                    .bind(run)
                    .fetch_one(&mut **tx)
                    .await?;
            if method == "item/fileChange/requestApproval"
                && settings["approval"] == "allow_edits"
                && settings["plan"] != true
            {
                sqlx::query("INSERT INTO native_requests(org_id,sandbox_id,request_id,run_id,method,params,answer) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING").bind(lease.org_id).bind(sandbox).bind(event["id"].to_string()).bind(run).bind(method).bind(p).bind(json!({"decision":"accept"})).execute(&mut **tx).await?;
                return Ok(false);
            }
            let question = method == "item/tool/requestUserInput";
            let request = event["id"].to_string();
            let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_requests WHERE org_id=$1 AND sandbox_id=$2 AND request_id=$3)").bind(lease.org_id).bind(sandbox).bind(&request).fetch_one(&mut **tx).await?;
            if !exists {
                let action = Uuid::now_v7();
                let summary = if question {
                    p["questions"].to_string()
                } else {
                    format!(
                        "{}\n{}",
                        p["command"].as_str().unwrap_or("文件变更"),
                        p["reason"].as_str().unwrap_or("")
                    )
                };
                let row:ActionRow=sqlx::query_as("INSERT INTO pending_actions(id,org_id,run_id,run_epoch,kind,summary,options,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,now()+interval '10 minutes') RETURNING id,run_id,run_epoch,summary,options,status,resolved_device,expires_at").bind(action).bind(lease.org_id).bind(run).bind(lease.epoch).bind(if question{"question"}else{"approval"}).bind(summary).bind(if question{vec!["__answer__"]}else{vec!["allow","deny"]}).fetch_one(&mut **tx).await?;
                sqlx::query("INSERT INTO native_requests(org_id,sandbox_id,request_id,run_id,method,params,action_id) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(lease.org_id).bind(sandbox).bind(request).bind(run).bind(method).bind(p).bind(action).execute(&mut **tx).await?;
                append(
                    tx,
                    lease.org_id,
                    &lease.channel(),
                    Some(lease),
                    "",
                    Event::PendingActionChanged(row.into()),
                )
                .await?;
                sqlx::query("UPDATE runs SET state='requires_action' WHERE org_id=$1 AND id=$2")
                    .bind(lease.org_id)
                    .bind(run)
                    .execute(&mut **tx)
                    .await?;
                append(
                    tx,
                    lease.org_id,
                    &lease.channel(),
                    Some(lease),
                    "",
                    Event::RunStateChanged(RunState {
                        run_id: run.to_string(),
                        state: "requires_action".into(),
                        exec_epoch: lease.epoch as u64,
                        read_only: false,
                    }),
                )
                .await?;
            }
        } else {
            // Unsupported requests are explicitly denied; never auto-grant permissions.
            let result = if method == "item/permissions/requestApproval" {
                json!({"permissions":{},"scope":"turn"})
            } else {
                json!({"decision":"decline"})
            };
            sqlx::query("INSERT INTO native_requests(org_id,sandbox_id,request_id,run_id,method,params,answer) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING").bind(lease.org_id).bind(sandbox).bind(event["id"].to_string()).bind(run).bind(method).bind(p).bind(result).execute(&mut **tx).await?;
        }
    }
    if matches!(method, "item/started" | "item/completed") {
        let native = &p["item"];
        // The platform already committed the user's input before calling the harness.
        if native["type"] == "userMessage" {
            return Ok(false);
        }
        let nid = native["id"].as_str().context("NATIVE_ITEM_ID")?;
        let mapped: Option<Uuid> = sqlx::query_scalar(
            "SELECT item_id FROM native_items WHERE org_id=$1 AND sandbox_id=$2 AND native_id=$3",
        )
        .bind(lease.org_id)
        .bind(sandbox)
        .bind(nid)
        .fetch_optional(&mut **tx)
        .await?;
        let typ = native["type"].as_str().unwrap_or("unknown");
        let mut item = if let Some(id) = mapped {
            commands::item(tx, lease.org_id, id).await?
        } else {
            let primary: Uuid =
                sqlx::query_scalar("SELECT assistant_item_id FROM runs WHERE org_id=$1 AND id=$2")
                    .bind(lease.org_id)
                    .bind(run)
                    .fetch_one(&mut **tx)
                    .await?;
            let used: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM native_items WHERE org_id=$1 AND item_id=$2)",
            )
            .bind(lease.org_id)
            .bind(primary)
            .fetch_one(&mut **tx)
            .await?;
            let id = if typ == "agentMessage" && !used {
                primary
            } else {
                Uuid::now_v7()
            };
            let mut item = if id == primary {
                commands::item(tx, lease.org_id, id).await?
            } else {
                Item {
                    id: id.to_string(),
                    revision: 0,
                    kind: match typ {
                        "agentMessage" => "assistant",
                        "userMessage" => "notice",
                        "reasoning" => "reasoning",
                        "plan" => "plan",
                        "fileChange" => "file_change",
                        _ => "tool",
                    }
                    .into(),
                    run_id: run.to_string(),
                    ..Default::default()
                }
            };
            item.revision += 1;
            item.state = "streaming".into();
            let item = committed_item(tx, lease, item).await?;
            sqlx::query(
                "INSERT INTO native_items(org_id,sandbox_id,native_id,item_id) VALUES($1,$2,$3,$4)",
            )
            .bind(lease.org_id)
            .bind(sandbox)
            .bind(nid)
            .bind(id)
            .execute(&mut **tx)
            .await?;
            item
        };
        item.revision += 1;
        item.state = if method == "item/completed" {
            "completed"
        } else {
            "streaming"
        }
        .into();
        item.text = match typ {
            "agentMessage" | "plan" => native["text"].as_str().unwrap_or("").into(),
            "commandExecution" => format!(
                "{}\n{}",
                native["command"].as_str().unwrap_or(""),
                native["aggregatedOutput"].as_str().unwrap_or("")
            ),
            "reasoning" => native["summary"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
            "userMessage" => "输入已提交给 Codex".into(),
            "fileChange" => native["changes"].to_string(),
            "mcpToolCall" => format!("MCP {} / {}", native["server"], native["tool"]),
            _ => format!("Codex: {typ}"),
        };
        if let Some(message) = native["nativeMessageId"].as_str() {
            sqlx::query("UPDATE native_items SET native_message_id=$4 WHERE org_id=$1 AND sandbox_id=$2 AND native_id=$3").bind(lease.org_id).bind(sandbox).bind(nid).bind(message).execute(&mut **tx).await?;
        }
        committed_item(tx, lease, item).await?;
    }
    if method == "turn/completed" {
        let status = p["turn"]["status"].as_str().unwrap_or("failed");
        let queued:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs r JOIN pending_inputs i ON i.org_id=r.org_id AND i.run_id=r.id WHERE r.org_id=$1 AND r.id=$2 AND r.harness='claude' AND r.state='running' AND i.consumed_step IS NULL)").bind(lease.org_id).bind(run).fetch_one(&mut **tx).await?;
        if queued && status == "completed" {
            sqlx::query("UPDATE runs SET native_turn_id=NULL WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(run)
                .execute(&mut **tx)
                .await?;
            return Ok(false);
        }
        chat::finish(
            tx,
            lease,
            run,
            match status {
                "completed" => "completed",
                "interrupted" => "cancelled",
                _ => "failed",
            },
            (status == "failed").then_some("NATIVE_TURN_FAILED"),
        )
        .await?;
        return Ok(true);
    }
    if method == "harness/exited" {
        anyhow::bail!("NATIVE_OUTCOME_UNKNOWN");
    }
    Ok(false)
}

pub async fn interrupt_terminal(app: &App, lease: &Lease) -> anyhow::Result<()> {
    let mut tx = app.store.tx(lease.org_id).await?;
    let rows:Vec<(Uuid,Uuid,String,String)>=sqlx::query_as("SELECT s.id,r.id,s.native_thread_id,r.native_turn_id FROM sandboxes s JOIN runs r ON r.org_id=s.org_id AND r.id=s.run_id WHERE s.org_id=$1 AND s.conversation_id=$2 AND s.persistent AND s.state='running' AND r.state IN ('cancelled','failed') AND s.native_thread_id IS NOT NULL AND r.native_turn_id IS NOT NULL").bind(lease.org_id).bind(lease.conversation_id).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    for (id, run, thread, turn) in rows {
        sandbox::guest(app, lease, id, GuestCommand::Fence).await?;
        sandbox::guest(
            app,
            lease,
            id,
            GuestCommand::NativeRequest {
                op_id: format!("{run}:interrupt"),
                method: "turn/interrupt".into(),
                params: json!({"threadId":thread,"turnId":turn}),
            },
        )
        .await?;
    }
    Ok(())
}

pub(crate) async fn run_settings(
    app: &App,
    lease: &Lease,
    run: Uuid,
) -> anyhow::Result<crate::run_config::RunConfig> {
    let mut tx = app.store.tx(lease.org_id).await?;
    let value: Value = sqlx::query_scalar("SELECT run_config FROM runs WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
    crate::run_config::RunConfig::resolve(value, "work", &app.config)
}
