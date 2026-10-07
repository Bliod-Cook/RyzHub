//! Chat tool execution. Each call moves planned → started → completed; a call that may have
//! reached the sandbox before a crash stays `started` and is resolved from the guest's durable
//! operation record, never re-run.
use anyhow::{Context, ensure};
use ryzhub_execution::{ExecSpec, GuestCommand};
use ryzhub_proto::{Item, ItemDetail, ToolDetail, item_detail};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    app::App,
    error::{Code, code_of, is_fenced},
    owner::OwnerLink,
    runs::{new_action, request_action},
    sandbox,
    store::{Lease, Tx, load_item, upsert_item},
};

/// How long one guest poll may wait for a running command to finish.
const EXEC_POLL_MS: u64 = 5000;

/// The Chat engine's tools, in the model's tool-definition format.
pub fn definitions() -> Value {
    let string = || json!({"type": "string"});
    let mut definitions = json!([
        tool(
            "project_file_search",
            "Search text in the current project knowledge files. Read only, returns file names and line numbers.",
            json!({"query": string()}),
            &["query"],
        ),
        tool(
            "exec",
            "Execute a shell command in the sandbox. Only /workspace persists; installed system \
             packages may be discarded. Full output is saved on the execution node. PTY is unsupported.",
            json!({
                "cmd": string(),
                "cwd": string(),
                "timeout_ms": {"type": "integer"},
                "background": {"type": "boolean"},
                "env": {"type": "object", "additionalProperties": string()},
            }),
            &["cmd"],
        ),
        tool(
            "exec_poll",
            "Read the status of a background execution handle without changing it.",
            json!({"handle": string()}),
            &["handle"],
        ),
        tool(
            "exec_write",
            "Write to the stdin of a running job.",
            json!({"handle": string(), "data": string()}),
            &["handle", "data"],
        ),
        tool(
            "exec_kill",
            "Stop a running job.",
            json!({"handle": string()}),
            &["handle"],
        ),
        tool(
            "view_image",
            "Look at an image file in the sandbox (PNG, JPEG, GIF, WebP or BMP), such as a chart you \
             rendered. Relative paths start at /workspace. Images over 2000 px on the long side are \
             scaled down. The image stays in the conversation for later turns.",
            json!({"path": string()}),
            &["path"],
        ),
        tool(
            "ask_user",
            "Ask the user a question and wait for a persisted response.",
            json!({"question": string(), "options": {"type": "array", "items": string()}}),
            &["question"],
        ),
    ]);
    definitions
        .as_array_mut()
        .unwrap()
        .extend(crate::memory::definitions());
    definitions
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "input_schema": {"type": "object", "properties": properties, "required": required},
    })
}

const KNOWN: [&str; 7] = [
    "exec",
    "exec_poll",
    "exec_write",
    "exec_kill",
    "view_image",
    "ask_user",
    "project_file_search",
];
/// Tools run silently; only a question waits for the user.
fn waits_for_user(name: &str) -> bool {
    name == "ask_user"
}

/// Execute every unfinished tool call of the run, in order. Returns false when a call waits
/// for the user; the Owner restarts the loop once the action is resolved.
pub async fn advance(
    app: &App,
    lease: &Lease,
    run: Uuid,
    link: &OwnerLink,
) -> anyhow::Result<bool> {
    loop {
        let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
        let row = sqlx::query(
            "SELECT call_id,name,arguments,status,action_id FROM tool_calls
             WHERE org_id=$1 AND run_id=$2 AND status<>'completed' ORDER BY step_no,call_id LIMIT 1",
        )
        .bind(lease.org_id)
        .bind(run)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(true);
        };
        let call: String = row.get("call_id");
        let name: String = row.get("name");
        let args: Value = row.get("arguments");
        let status: String = row.get("status");
        if !KNOWN.contains(&name.as_str()) && !crate::memory::TOOLS.contains(&name.as_str()) {
            tx.commit().await?;
            record(
                app,
                lease,
                run,
                &call,
                json!({"error":"unknown_tool"}),
                link,
            )
            .await?;
            continue;
        }
        if status == "planned" && waits_for_user(&name) {
            match row.get::<Option<Uuid>, _>("action_id") {
                Some(action) => {
                    let decision: String = sqlx::query_scalar(
                        "SELECT status FROM pending_actions WHERE org_id=$1 AND id=$2",
                    )
                    .bind(lease.org_id)
                    .bind(action)
                    .fetch_one(&mut *tx)
                    .await?;
                    if decision == "deny" {
                        tx.commit().await?;
                        record(app, lease, run, &call, json!({"error":"user_denied"}), link)
                            .await?;
                        continue;
                    }
                    if decision != "allow" && decision != "answered" {
                        // Still pending (or invalidated): nothing to do until it changes.
                        tx.commit().await?;
                        return Ok(false);
                    }
                }
                None => {
                    ask(app, &mut tx, lease, run, &call, &args).await?;
                    tx.commit().await?;
                    link.committed().await;
                    return Ok(false);
                }
            }
        }
        if status == "planned" {
            sqlx::query(
                "UPDATE tool_calls SET status='started',started_at=now() WHERE org_id=$1 AND run_id=$2 AND call_id=$3",
            )
            .bind(lease.org_id)
            .bind(run)
            .bind(&call)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        match dispatch(app, lease, run, &call, &name, args, link).await {
            Ok(value) => record(app, lease, run, &call, value, link).await?,
            Err(error) if is_fenced(&error) => return Err(error),
            Err(error) if crate::memory::TOOLS.contains(&name.as_str()) => {
                record(
                    app,
                    lease,
                    run,
                    &call,
                    json!({"error":crate::error::public_code(&error).as_str()}),
                    link,
                )
                .await?;
            }
            // Argument and capability errors are answers the model can act on. Anything else
            // may have reached the guest: keep `started` so the next attempt asks the guest.
            Err(error) => match code_of(&error) {
                Some(
                    code @ (Code::SandboxDisabled
                    | Code::InvalidToolArguments
                    | Code::PtyUnsupported
                    | Code::PolicyDenied
                    | Code::Forbidden
                    | Code::InvalidArguments),
                ) => record(app, lease, run, &call, json!({"error":code.as_str()}), link).await?,
                _ => return Err(error),
            },
        }
    }
}

/// Create the question of a planned `ask_user` call and pause the run.
async fn ask(
    app: &App,
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    call: &str,
    args: &Value,
) -> anyhow::Result<()> {
    let summary = format!(
        "问题：{}",
        args["question"].as_str().unwrap_or("请提供更多信息")
    );
    ensure!(summary.len() <= 65_536, Code::InvalidToolArguments);
    let action = new_action(
        tx,
        lease,
        run,
        "question",
        &summary,
        &["__answer__"],
        &app.config,
    )
    .await?;
    sqlx::query("UPDATE tool_calls SET action_id=$4 WHERE org_id=$1 AND run_id=$2 AND call_id=$3")
        .bind(lease.org_id)
        .bind(run)
        .bind(call)
        .bind(action.id)
        .execute(&mut **tx)
        .await?;
    request_action(tx, lease, run, action).await
}

async fn dispatch(
    app: &App,
    lease: &Lease,
    run: Uuid,
    call: &str,
    name: &str,
    args: Value,
    link: &OwnerLink,
) -> anyhow::Result<Value> {
    if crate::memory::TOOLS.contains(&name) {
        let vector = crate::memory::prepare_search(app, lease.org_id, run, name, &args).await?;
        let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
        let result = crate::memory::execute(
            &mut tx,
            app,
            lease.org_id,
            run,
            name,
            args,
            format!("chat:{run}:{call}"),
            vector,
        )
        .await?;
        crate::memory::project(&mut tx, lease).await?;
        tx.commit().await?;
        return Ok(result);
    }
    if name == "project_file_search" {
        let query = args["query"].as_str().context(Code::InvalidToolArguments)?;
        let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
        return crate::projects::search(&mut tx, lease.org_id, run, query).await;
    }
    let sandbox = sandbox::ensure_sandbox(app, lease, run, link).await?;
    let (status, recorded): (String, Option<Value>) = {
        let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
        let row = sqlx::query_as(
            "UPDATE tool_calls SET sandbox_id=$4 WHERE org_id=$1 AND run_id=$2 AND call_id=$3
             RETURNING status,result",
        )
        .bind(lease.org_id)
        .bind(run)
        .bind(call)
        .bind(sandbox)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        row
    };
    // The sandbox sync may just have recorded this call: it finished before a crash.
    if status == "completed" {
        return Ok(recorded.unwrap_or_default());
    }
    let op = format!("{run}:{call}");
    let value = match name {
        "exec" => {
            let spec: ExecSpec =
                serde_json::from_value(args).context(Code::InvalidToolArguments)?;
            let background = spec.background;
            let command = GuestCommand::Exec {
                op_id: op.clone(),
                spec,
            };
            let mut result = sandbox::guest(app, lease, sandbox, command).await?;
            if !background {
                while result["state"] == "running" {
                    let tx = crate::runs::guarded(&app.store, lease, run).await?;
                    tx.commit().await?;
                    let poll = GuestCommand::Poll {
                        op_id: op.clone(),
                        wait_ms: EXEC_POLL_MS,
                    };
                    result = sandbox::guest(app, lease, sandbox, poll).await?;
                }
                sandbox::sync_events(app, lease, run, sandbox, link).await?;
            }
            result
        }
        "exec_poll" | "exec_write" | "exec_kill" => {
            let handle = args["handle"]
                .as_str()
                .context(Code::InvalidToolArguments)?
                .to_owned();
            ensure!(
                handle.starts_with(&format!("{run}:")),
                Code::InvalidToolArguments
            );
            let command = match name {
                "exec_poll" => GuestCommand::Poll {
                    op_id: handle,
                    wait_ms: 0,
                },
                "exec_write" => GuestCommand::Write {
                    op_id: handle,
                    request_id: op.clone(),
                    data: args["data"]
                        .as_str()
                        .context(Code::InvalidToolArguments)?
                        .into(),
                },
                _ => GuestCommand::Kill {
                    op_id: handle,
                    request_id: op.clone(),
                },
            };
            sandbox::guest(app, lease, sandbox, command).await?
        }
        "view_image" => super::media::view(app, lease, run, sandbox, &args).await?,
        _ => anyhow::bail!(Code::InvalidToolArguments),
    };
    Ok(if value["state"] == "outcome_unknown" {
        json!({"error":"outcome_unknown","details":value})
    } else {
        value
    })
}

/// Tool detail with the call's result attached.
pub fn with_result(mut item: Item, result: &Value) -> Item {
    let mut tool = match item.detail.take().and_then(|d| d.kind) {
        Some(item_detail::Kind::Tool(tool)) => tool,
        _ => ToolDetail::default(),
    };
    tool.result_json = result.to_string();
    let output = [&result["stdout_head"], &result["stdout_tail"]]
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>()
        .join("");
    if !output.is_empty() {
        tool.output = output;
    }
    item.detail = Some(ItemDetail {
        kind: Some(item_detail::Kind::Tool(tool)),
    });
    item
}

/// Complete a call once; later duplicates are ignored.
async fn record(
    app: &App,
    lease: &Lease,
    run: Uuid,
    call: &str,
    result: Value,
    link: &OwnerLink,
) -> anyhow::Result<()> {
    let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
    let item: Option<Uuid> = sqlx::query_scalar(
        "UPDATE tool_calls SET status='completed',result=$4,completed_at=now()
         WHERE org_id=$1 AND run_id=$2 AND call_id=$3 AND status<>'completed' RETURNING item_id",
    )
    .bind(lease.org_id)
    .bind(run)
    .bind(call)
    .bind(&result)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(item) = item {
        complete_item(&mut tx, lease, item, &result).await?;
    }
    tx.commit().await?;
    link.committed().await;
    Ok(())
}

/// Finalize a tool item with its result. `text` keeps the v1 rendering (the result JSON).
pub async fn complete_item(
    tx: &mut Tx<'_>,
    lease: &Lease,
    item: Uuid,
    result: &Value,
) -> anyhow::Result<()> {
    let mut item = load_item(tx, lease.org_id, item).await?;
    item.revision += 1;
    item.state = "completed".into();
    item.text = result.to_string();
    upsert_item(tx, lease, with_result(item, result)).await?;
    Ok(())
}

/// The user's answer is the result of the `ask_user` call that asked it.
pub async fn answer_question(
    tx: &mut Tx<'_>,
    lease: &Lease,
    action: Uuid,
    answer: &str,
) -> anyhow::Result<()> {
    let result = json!({"answer":answer});
    let item: Uuid = sqlx::query_scalar(
        "UPDATE tool_calls SET result=$3,status='completed',completed_at=now()
         WHERE org_id=$1 AND action_id=$2 RETURNING item_id",
    )
    .bind(lease.org_id)
    .bind(action)
    .bind(&result)
    .fetch_one(&mut **tx)
    .await?;
    complete_item(tx, lease, item, &result).await
}
