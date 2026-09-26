use crate::{
    config::Config,
    store::{ActionRow, Lease, Tx, append},
};
use anyhow::{Context, ensure};
use ryzhub_proto::{command_envelope::Command, event_envelope::Event, frame::Body, *};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

pub async fn execute(
    tx: &mut Tx<'_>,
    lease: &Lease,
    user: Uuid,
    device: &str,
    cmd: &CommandEnvelope,
    config: &Config,
) -> anyhow::Result<Body> {
    match cmd.command.as_ref().context("INVALID_COMMAND")? {
        Command::UpdateRunConfig(input) => {
            crate::conversation_commands::configure(tx, lease, &input.config_json, config, &cmd.cid)
                .await?
        }
        Command::Compact(_) => {
            let (mode, value): (String, Value) = sqlx::query_as(
                "SELECT mode,run_config FROM conversations WHERE org_id=$1 AND id=$2",
            )
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut **tx)
            .await?;
            let settings = crate::run_config::RunConfig::resolve(value, &mode, config)?;
            ensure!(
                crate::run_config::capabilities(&settings.harness, &settings.protocol)["compact"]
                    != "unsupported",
                "COMPACT_UNSUPPORTED"
            );
            create(tx, lease, cmd, "/compact", false, config).await?;
            sqlx::query("UPDATE runs SET operation='compact' WHERE org_id=$1 AND conversation_id=$2 AND cause_cid=$3").bind(lease.org_id).bind(lease.conversation_id).bind(Uuid::parse_str(&cmd.cid)?).execute(&mut **tx).await?;
        }
        Command::Fork(input) => {
            crate::conversation_commands::enqueue_fork(tx, lease, &input.item_id, &cmd.cid).await?
        }
        Command::SendMessage(input) => create(tx, lease, cmd, &input.text, false, config).await?,
        Command::SideQuery(input) => create(tx, lease, cmd, &input.text, true, config).await?,
        Command::Steer(input) => {
            valid_text(&input.text)?;
            let run:Uuid=sqlx::query_scalar("SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2 AND NOT read_only AND state IN ('running','requires_action')").bind(lease.org_id).bind(lease.conversation_id).fetch_optional(&mut **tx).await?.context("NO_ACTIVE_RUN")?;
            add_input(tx, lease, run, &input.text, "steer", &cmd.cid).await?;
        }
        Command::DecideApproval(decision) => {
            ensure!(
                matches!(decision.decision.as_str(), "allow" | "deny"),
                "INVALID_DECISION"
            );
            resolve(
                tx,
                lease,
                user,
                device,
                &decision.action_id,
                decision.run_epoch,
                "approval",
                &decision.decision,
                &cmd.cid,
            )
            .await?;
        }
        Command::AnswerQuestion(answer) => {
            valid_text(&answer.answer)?;
            resolve(
                tx,
                lease,
                user,
                device,
                &answer.action_id,
                answer.run_epoch,
                "question",
                &answer.answer,
                &cmd.cid,
            )
            .await?;
        }
        Command::Cancel(_) => {
            let runs:Vec<Uuid>=sqlx::query_scalar("SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('running','requires_action')").bind(lease.org_id).bind(lease.conversation_id).fetch_all(&mut **tx).await?;
            for run in runs {
                super::finish(tx, lease, run, "cancelled", None).await?;
            }
        }
        _ => anyhow::bail!("UNSUPPORTED_COMMAND"),
    }
    let seq: i64 = sqlx::query_scalar("SELECT last_seq FROM channels WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(lease.channel())
        .fetch_one(&mut **tx)
        .await?;
    Ok(Body::CommandAck(CommandAck {
        cid: cmd.cid.clone(),
        accepted_seq: seq as u64,
        result: "accepted".into(),
    }))
}
fn valid_text(text: &str) -> anyhow::Result<()> {
    ensure!(
        !text.trim().is_empty() && text.len() <= 65536,
        "INVALID_MESSAGE"
    );
    Ok(())
}
async fn create(
    tx: &mut Tx<'_>,
    lease: &Lease,
    cmd: &CommandEnvelope,
    text: &str,
    read_only: bool,
    config: &Config,
) -> anyhow::Result<()> {
    valid_text(text)?;
    let mode: String =
        sqlx::query_scalar("SELECT mode FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut **tx)
            .await?;
    let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversation_operations WHERE org_id=$1 AND conversation_id=$2 AND state IN ('queued','running'))").bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut **tx).await?;
    ensure!(!busy, "CONVERSATION_BUSY");
    let work = mode == "work";
    if work {
        crate::work::available(tx, lease).await?;
        let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2 AND kind IN ('archive','restore') AND state IN ('queued','running'))").bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut **tx).await?;
        ensure!(!busy, "WORKSPACE_BUSY");
    }

    let value: Value =
        sqlx::query_scalar("SELECT run_config FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut **tx)
            .await?;
    let mut run_config = crate::run_config::RunConfig::resolve(value, &mode, config)?;
    if work && read_only {
        run_config.harness = "chat".into();
    }
    if !(work && read_only) {
        run_config.validate(tx, lease.org_id, config).await?;
    }
    let model = &run_config.model;
    let harness = run_config.harness.as_str();
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(lease.org_id)
        .fetch_one(&mut **tx)
        .await?;
    let org_policy: Option<Value> = sqlx::query_scalar(
        "SELECT policy FROM org_policies WHERE org_id=$1 ORDER BY version DESC LIMIT 1",
    )
    .bind(lease.org_id)
    .fetch_optional(&mut **tx)
    .await?;
    let policy = config
        .policy
        .restrict(&serde_json::from_value(org_policy.unwrap_or(json!({})))?);
    ensure!(
        policy.models.as_ref().is_none_or(|m| m.contains(model))
            && policy
                .harnesses
                .as_ref()
                .is_none_or(|h| h.contains(harness)),
        "POLICY_DENIED"
    );
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM runs WHERE org_id=$1 AND state IN ('running','requires_action')",
    )
    .bind(lease.org_id)
    .fetch_one(&mut **tx)
    .await?;
    ensure!(
        policy
            .max_sandboxes
            .is_none_or(|limit| active < limit as i64),
        "ORG_CONCURRENCY_LIMIT"
    );
    if !read_only {
        let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE org_id=$1 AND conversation_id=$2 AND NOT read_only AND state IN ('running','requires_action'))").bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut **tx).await?;
        ensure!(!busy, "RUN_BUSY_USE_STEER");
    }
    let seed:Option<(Uuid,i32,Value,Option<Value>)>=sqlx::query_as("SELECT r.id,s.step_no,s.request_messages,s.native_response FROM loop_steps s JOIN runs r ON r.org_id=s.org_id AND r.id=s.run_id WHERE s.org_id=$1 AND r.conversation_id=$2 AND r.protocol=$3 AND NOT r.read_only AND r.state IN ('running','requires_action','completed') ORDER BY r.started_at DESC,s.step_no DESC LIMIT 1").bind(lease.org_id).bind(lease.conversation_id).bind(&run_config.protocol).fetch_optional(&mut **tx).await?;
    let mut messages: Value = sqlx::query_scalar::<_, Option<Value>>(
        "SELECT context_seed FROM conversations WHERE org_id=$1 AND id=$2",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut **tx)
    .await?
    .unwrap_or(json!([]));
    if let Some((run, step, request, response)) = seed {
        messages = request;
        if let Some(response) = response {
            let results:Vec<(String,Value)>=sqlx::query_as("SELECT call_id,result FROM tool_calls WHERE org_id=$1 AND run_id=$2 AND step_no=$3 AND status='completed' ORDER BY call_id").bind(lease.org_id).bind(run).bind(step).fetch_all(&mut **tx).await?;
            super::protocol::continue_with(
                &run_config.protocol,
                &mut messages,
                &response,
                &results,
            )?;
        }
    }
    if work && read_only {
        let texts:Vec<String>=sqlx::query_scalar("SELECT text FROM items WHERE org_id=$1 AND conversation_id=$2 AND kind IN ('user','assistant') AND state='completed' ORDER BY first_seq DESC LIMIT 20").bind(lease.org_id).bind(lease.conversation_id).fetch_all(&mut **tx).await?;
        let summary = texts.into_iter().rev().collect::<Vec<_>>().join("\n");
        messages = json!([{"role":"user","content":format!("Read-only side question. Main conversation summary (no filesystem access):\n{}",summary.chars().take(24000).collect::<String>())}]);
    }
    let parent: Option<Uuid> = if read_only {
        sqlx::query_scalar("SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2 AND NOT read_only ORDER BY started_at DESC LIMIT 1").bind(lease.org_id).bind(lease.conversation_id).fetch_optional(&mut **tx).await?
    } else {
        None
    };
    let credential: Option<Uuid> = sqlx::query_scalar(
        "SELECT model_credential_id FROM conversations WHERE org_id=$1 AND id=$2",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut **tx)
    .await?;
    let run = Uuid::now_v7();
    let item = Uuid::now_v7();
    sqlx::query("INSERT INTO runs(id,org_id,conversation_id,harness,model,exec_epoch,state,response_text,assistant_item_id,cause_cid,read_only,parent_run_id,seed_messages,credential_id) VALUES($1,$2,$3,$12,$4,$5,'running','',$6,$7,$8,$9,$10,$11)").bind(run).bind(lease.org_id).bind(lease.conversation_id).bind(model).bind(lease.epoch).bind(item).bind(Uuid::parse_str(&cmd.cid)?).bind(read_only).bind(parent).bind(messages).bind(credential).bind(harness).execute(&mut **tx).await?;
    sqlx::query("UPDATE runs SET protocol=$3,run_config=$4,project_id=(SELECT project_id FROM conversations WHERE org_id=$1 AND id=$5) WHERE org_id=$1 AND id=$2").bind(lease.org_id).bind(run).bind(&run_config.protocol).bind(serde_json::to_value(&run_config)?).bind(lease.conversation_id).execute(&mut **tx).await?;
    // Freeze project content with the run. Edits and moves only affect future runs.
    sqlx::query("UPDATE runs r SET project_context=coalesce((SELECT E'\\n\\n<project>Memory boundary: ' || p.memory_mode || E'>\\n' || p.instructions || E'\\nKnowledge files are available read-only in /project and via project_file_search.\\n</project>' FROM projects p WHERE p.org_id=r.org_id AND p.id=r.project_id AND p.state='active'),''),project_files=coalesce((SELECT jsonb_object_agg(f.name,f.content) FROM project_files f WHERE f.org_id=r.org_id AND f.project_id=r.project_id),'{}') WHERE r.org_id=$1 AND r.id=$2").bind(lease.org_id).bind(run).execute(&mut **tx).await?;
    add_input(tx, lease, run, text, "message", &cmd.cid).await?;
    append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        &cmd.cid,
        Event::ItemUpserted(Item {
            id: item.to_string(),
            revision: 1,
            kind: if read_only { "side_query" } else { "assistant" }.into(),
            state: "streaming".into(),
            run_id: run.to_string(),
            ..Default::default()
        }),
    )
    .await?;
    append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        &cmd.cid,
        Event::RunStateChanged(RunState {
            read_only,
            run_id: run.to_string(),
            state: "running".into(),
            exec_epoch: lease.epoch as u64,
        }),
    )
    .await?;
    Ok(())
}
async fn add_input(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    text: &str,
    kind: &str,
    cid: &str,
) -> anyhow::Result<()> {
    let item = Uuid::now_v7();
    append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        cid,
        Event::ItemUpserted(Item {
            id: item.to_string(),
            revision: 1,
            kind: if kind == "steer" { "steer" } else { "user" }.into(),
            state: if kind == "steer" {
                "accepted"
            } else {
                "completed"
            }
            .into(),
            text: text.into(),
            run_id: run.to_string(),
            ..Default::default()
        }),
    )
    .await?;
    sqlx::query(
        "INSERT INTO pending_inputs(org_id,run_id,text,kind,item_id) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(lease.org_id)
    .bind(run)
    .bind(text)
    .bind(kind)
    .bind(item)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
#[allow(clippy::too_many_arguments)]
async fn resolve(
    tx: &mut Tx<'_>,
    lease: &Lease,
    user: Uuid,
    device: &str,
    id: &str,
    epoch: u64,
    kind: &str,
    answer: &str,
    cid: &str,
) -> anyhow::Result<()> {
    let id = Uuid::parse_str(id).context("INVALID_ACTION")?;
    let action:ActionRow=sqlx::query_as("UPDATE pending_actions a SET status=$5,resolved_by=$6,resolved_device=$7 FROM runs r WHERE a.org_id=$1 AND a.id=$2 AND a.run_epoch=$3 AND a.status='pending' AND a.kind=$8 AND a.expires_at>now() AND r.org_id=a.org_id AND r.id=a.run_id AND r.conversation_id=$4 AND r.exec_epoch=$3 AND r.state='requires_action' RETURNING a.id,a.run_id,a.run_epoch,a.summary,a.options,a.status,a.resolved_device,a.expires_at").bind(lease.org_id).bind(id).bind(epoch as i64).bind(lease.conversation_id).bind(if kind=="question"{"answered"}else{answer}).bind(user).bind(device).bind(kind).fetch_optional(&mut **tx).await?.context("ACTION_ALREADY_RESOLVED")?;
    let run = action.run_id;
    let native: Option<Value> =
        sqlx::query_scalar("SELECT params FROM native_requests WHERE org_id=$1 AND action_id=$2")
            .bind(lease.org_id)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(params) = native {
        let response = crate::work::answer(&params, kind, answer)?;
        sqlx::query("UPDATE native_requests SET answer=$3 WHERE org_id=$1 AND action_id=$2")
            .bind(lease.org_id)
            .bind(id)
            .bind(response)
            .execute(&mut **tx)
            .await?;
    } else if kind == "question" {
        let result = json!({"answer":answer});
        let item_id: Uuid = sqlx::query_scalar("UPDATE tool_calls SET result=$3,status='completed',completed_at=now() WHERE org_id=$1 AND action_id=$2 RETURNING item_id").bind(lease.org_id).bind(id).bind(&result).fetch_one(&mut **tx).await?;
        let mut item = item(tx, lease.org_id, item_id).await?;
        item.revision += 1;
        item.state = "completed".into();
        item.text = result.to_string();
        super::committed_item(tx, lease, item).await?;
    }
    append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        cid,
        Event::PendingActionChanged(action.into()),
    )
    .await?;
    let state: String = sqlx::query_scalar("UPDATE runs r SET state=CASE WHEN EXISTS(SELECT 1 FROM pending_actions a WHERE a.org_id=r.org_id AND a.run_id=r.id AND a.status='pending') THEN 'requires_action' ELSE 'running' END WHERE r.org_id=$1 AND r.id=$2 RETURNING state")
        .bind(lease.org_id).bind(run).fetch_one(&mut **tx).await?;
    append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        cid,
        Event::RunStateChanged(RunState {
            read_only: false,
            run_id: run.to_string(),
            state,
            exec_epoch: lease.epoch as u64,
        }),
    )
    .await?;
    Ok(())
}

pub async fn item(tx: &mut Tx<'_>, org: Uuid, id: Uuid) -> anyhow::Result<Item> {
    let row=sqlx::query("SELECT id,revision,kind,state,text,first_seq,last_seq,run_id FROM items WHERE org_id=$1 AND id=$2").bind(org).bind(id).fetch_one(&mut **tx).await?;
    Ok(Item {
        id: row.get::<Uuid, _>("id").to_string(),
        revision: row.get::<i64, _>("revision") as u64,
        kind: row.get("kind"),
        state: row.get("state"),
        text: row.get("text"),
        first_seq: row.get::<i64, _>("first_seq") as u64,
        last_seq: row.get::<i64, _>("last_seq") as u64,
        run_id: row.get::<Uuid, _>("run_id").to_string(),
    })
}
