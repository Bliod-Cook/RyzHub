use super::{commands, committed_item, guard};
use crate::{
    api::App,
    sandbox,
    store::{ActionRow, Lease, append},
};
use anyhow::{Context, ensure};
use ryzhub_execution::{ExecSpec, GuestCommand};
use ryzhub_proto::{RunState, event_envelope::Event};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

pub fn definitions() -> Value {
    json!([
        {"name":"project_file_search","description":"Search text in the current project knowledge files. Read only, returns file names and line numbers.","input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}},
        {"name":"exec","description":"Execute a shell command in the sandbox. User approval is required. Only /workspace persists; installed system packages may be discarded. Full output is saved on the execution node. PTY is unsupported.","input_schema":{"type":"object","properties":{"cmd":{"type":"string"},"cwd":{"type":"string"},"timeout_ms":{"type":"integer"},"background":{"type":"boolean"},"env":{"type":"object","additionalProperties":{"type":"string"}}},"required":["cmd"]}},
        {"name":"exec_poll","description":"Read the status of a background execution handle without changing it.","input_schema":{"type":"object","properties":{"handle":{"type":"string"}},"required":["handle"]}},
        {"name":"exec_write","description":"Write to the stdin of a running job. User approval is required.","input_schema":{"type":"object","properties":{"handle":{"type":"string"},"data":{"type":"string"}},"required":["handle","data"]}},
        {"name":"exec_kill","description":"Stop a running job. User approval is required.","input_schema":{"type":"object","properties":{"handle":{"type":"string"}},"required":["handle"]}},
        {"name":"ask_user","description":"Ask the user a question and wait for a persisted response.","input_schema":{"type":"object","properties":{"question":{"type":"string"},"options":{"type":"array","items":{"type":"string"}}},"required":["question"]}}
    ])
}
pub async fn advance(app: &App, lease: &Lease, run: Uuid) -> anyhow::Result<bool> {
    loop {
        let mut tx = app.store.tx(lease.org_id).await?;
        guard(&mut tx, lease, run).await?;
        let row=sqlx::query("SELECT call_id,name,arguments,status,action_id,item_id,sandbox_id FROM tool_calls WHERE org_id=$1 AND run_id=$2 AND status<>'completed' ORDER BY step_no,call_id LIMIT 1").bind(lease.org_id).bind(run).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(true);
        };
        let call: String = row.get("call_id");
        let name: String = row.get("name");
        let args: Value = row.get("arguments");
        let status: String = row.get("status");
        let action: Option<Uuid> = row.get("action_id");
        let known = matches!(
            name.as_str(),
            "exec" | "exec_poll" | "exec_write" | "exec_kill" | "ask_user" | "project_file_search"
        );
        if !known {
            tx.commit().await?;
            record(app, lease, run, &call, json!({"error":"unknown_tool"})).await?;
            continue;
        }
        let settings: Value =
            sqlx::query_scalar("SELECT run_config FROM runs WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(run)
                .fetch_one(&mut *tx)
                .await?;
        let auto = settings["approval"] == "full_auto" && name != "ask_user";
        if !matches!(name.as_str(), "exec_poll" | "project_file_search")
            && !auto
            && status == "planned"
        {
            if let Some(action) = action {
                let decision: String = sqlx::query_scalar(
                    "SELECT status FROM pending_actions WHERE org_id=$1 AND id=$2",
                )
                .bind(lease.org_id)
                .bind(action)
                .fetch_one(&mut *tx)
                .await?;
                if decision == "deny" {
                    tx.commit().await?;
                    record(app, lease, run, &call, json!({"error":"user_denied"})).await?;
                    continue;
                }
                ensure!(
                    decision == "allow" || decision == "answered",
                    "ACTION_NOT_RESOLVED"
                );
            } else {
                let question = name == "ask_user";
                let options = if question {
                    vec!["__answer__".to_owned()]
                } else {
                    vec!["allow".into(), "deny".into()]
                };
                let summary = if question {
                    format!(
                        "问题：{}",
                        args["question"].as_str().unwrap_or("请提供更多信息")
                    )
                } else {
                    format!("允许执行 {name}？\n{args}")
                };
                ensure!(summary.len() <= 65536, "TOOL_REQUEST_TOO_LARGE");
                let action:ActionRow=sqlx::query_as("INSERT INTO pending_actions(id,org_id,run_id,run_epoch,kind,summary,options,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,now()+interval '24 hours') RETURNING id,run_id,run_epoch,summary,options,status,resolved_device,expires_at").bind(Uuid::now_v7()).bind(lease.org_id).bind(run).bind(lease.epoch).bind(if question{"question"}else{"approval"}).bind(summary).bind(options).fetch_one(&mut *tx).await?;
                sqlx::query("UPDATE tool_calls SET action_id=$4 WHERE org_id=$1 AND run_id=$2 AND call_id=$3").bind(lease.org_id).bind(run).bind(&call).bind(action.id).execute(&mut *tx).await?;
                sqlx::query("UPDATE runs SET state='requires_action' WHERE org_id=$1 AND id=$2")
                    .bind(lease.org_id)
                    .bind(run)
                    .execute(&mut *tx)
                    .await?;
                append(
                    &mut tx,
                    lease.org_id,
                    &lease.channel(),
                    Some(lease),
                    "",
                    Event::PendingActionChanged(action.into()),
                )
                .await?;
                append(
                    &mut tx,
                    lease.org_id,
                    &lease.channel(),
                    Some(lease),
                    "",
                    Event::RunStateChanged(RunState {
                        read_only: false,
                        run_id: run.to_string(),
                        state: "requires_action".into(),
                        exec_epoch: lease.epoch as u64,
                    }),
                )
                .await?;
                tx.commit().await?;
                return Ok(false);
            }
        }
        if status == "planned" {
            sqlx::query("UPDATE tool_calls SET status='started',started_at=now() WHERE org_id=$1 AND run_id=$2 AND call_id=$3").bind(lease.org_id).bind(run).bind(&call).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        let result = dispatch(app, lease, run, &call, &name, args).await;
        match result {
            Ok(value) => record(app, lease, run, &call, value).await?,
            Err(error) if error.to_string() == "FENCED" => return Err(error),
            Err(error) => {
                // Dispatch may have reached the guest. Preserve started state on connectivity
                // failure so the next Owner queries the durable op_id, never inventing success.
                if !matches!(
                    error.to_string().as_str(),
                    "SANDBOX_DISABLED" | "INVALID_TOOL_ARGUMENTS" | "PTY_UNSUPPORTED"
                ) {
                    return Err(error);
                }
                record(app, lease, run, &call, json!({"error":error.to_string()})).await?;
            }
        }
    }
}
async fn dispatch(
    app: &App,
    lease: &Lease,
    run: Uuid,
    call: &str,
    name: &str,
    args: Value,
) -> anyhow::Result<Value> {
    if name == "project_file_search" {
        let mut tx = app.store.tx(lease.org_id).await?;
        guard(&mut tx, lease, run).await?;
        return crate::projects::search(
            &mut tx,
            lease.org_id,
            run,
            args["query"].as_str().context("INVALID_TOOL_ARGUMENTS")?,
        )
        .await;
    }
    let id = sandbox::ensure_sandbox(app, lease, run).await?;
    let mut tx = app.store.tx(lease.org_id).await?;
    guard(&mut tx, lease, run).await?;
    sqlx::query("UPDATE tool_calls SET sandbox_id=$4 WHERE org_id=$1 AND run_id=$2 AND call_id=$3")
        .bind(lease.org_id)
        .bind(run)
        .bind(call)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let op = format!("{run}:{call}");
    let value = match name {
        "exec" => {
            let spec: ExecSpec = serde_json::from_value(args).context("INVALID_TOOL_ARGUMENTS")?;
            let background = spec.background;
            let mut result = sandbox::guest(
                app,
                lease,
                id,
                GuestCommand::Exec {
                    op_id: op.clone(),
                    spec,
                },
            )
            .await?;
            if !background {
                while result["state"] == "running" {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let mut tx = app.store.tx(lease.org_id).await?;
                    guard(&mut tx, lease, run).await?;
                    tx.commit().await?;
                    sandbox::sync_events(app, lease, run, id).await?;
                    result =
                        sandbox::guest(app, lease, id, GuestCommand::Poll { op_id: op.clone() })
                            .await?;
                }
                sandbox::sync_events(app, lease, run, id).await?;
            }
            result
        }
        "exec_poll" | "exec_write" | "exec_kill" => {
            let handle = args["handle"]
                .as_str()
                .context("INVALID_TOOL_ARGUMENTS")?
                .to_owned();
            ensure!(
                handle.starts_with(&format!("{run}:")),
                "INVALID_TOOL_ARGUMENTS"
            );
            let command = match name {
                "exec_poll" => GuestCommand::Poll { op_id: handle },
                "exec_write" => GuestCommand::Write {
                    op_id: handle,
                    request_id: op.clone(),
                    data: args["data"]
                        .as_str()
                        .context("INVALID_TOOL_ARGUMENTS")?
                        .into(),
                },
                _ => GuestCommand::Kill {
                    op_id: handle,
                    request_id: op.clone(),
                },
            };
            sandbox::guest(app, lease, id, command).await?
        }
        _ => anyhow::bail!("INVALID_TOOL_ARGUMENTS"),
    };
    if value["state"] == "outcome_unknown" {
        Ok(json!({"error":"outcome_unknown","details":value}))
    } else {
        Ok(value)
    }
}
async fn record(
    app: &App,
    lease: &Lease,
    run: Uuid,
    call: &str,
    result: Value,
) -> anyhow::Result<()> {
    let mut tx = app.store.tx(lease.org_id).await?;
    guard(&mut tx, lease, run).await?;
    let item:Option<Uuid>=sqlx::query_scalar("UPDATE tool_calls SET status='completed',result=$4,completed_at=now() WHERE org_id=$1 AND run_id=$2 AND call_id=$3 AND status<>'completed' RETURNING item_id").bind(lease.org_id).bind(run).bind(call).bind(&result).fetch_optional(&mut *tx).await?;
    if let Some(item) = item {
        let mut item = commands::item(&mut tx, lease.org_id, item).await?;
        item.revision += 1;
        item.state = "completed".into();
        item.text = result.to_string();
        committed_item(&mut tx, lease, item).await?;
    }
    tx.commit().await?;
    Ok(())
}
