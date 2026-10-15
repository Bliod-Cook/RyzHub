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
    owner::{LiveRef, OwnerLink, WorkerEvent},
    runs::{new_action, request_action},
    sandbox,
    store::{Lease, Tx, load_item, upsert_item},
};

/// How long one guest poll may wait for a running command to finish.
const EXEC_POLL_MS: u64 = 5000;
/// How often a running command's output is shown while the call waits for it.
const OUTPUT_POLL_MS: u64 = 1000;
/// The most of a moved tool result one `read_tool_output` call returns.
const READ_PAGE_BYTES: usize = 32 * 1024;

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
             packages may be discarded. Full output is saved on the execution node.",
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
            "fs_upload",
            "Copy a file the user attached (listed under /attachments, which is read-only) into \
             the writable workspace. `path` is relative to /workspace; missing directories are \
             created.",
            json!({"name": string(), "path": string()}),
            &["name", "path"],
        ),
        tool(
            "fs_download",
            "Hand a sandbox file to the user: it is added to their file library and to this \
             conversation, and stays available after the sandbox is gone. Relative paths start \
             at /workspace. Returns the file's /attachments path; link to it in your reply.",
            json!({"path": string()}),
            &["path"],
        ),
        tool(
            "read_tool_output",
            "Read a tool result that was moved out of the context to save space, by the id its \
             placeholder names. Long results come in pages: pass the previous page's \
             `next_offset` as `offset`.",
            json!({"id": string(), "offset": {"type": "integer"}}),
            &["id"],
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

/// Reads a plugin skill (offered when the run's plugins have skills).
pub fn load_skill_definition() -> Value {
    tool(
        "load_skill",
        "Read a skill of an installed plugin before following it: its SKILL.md, or one of its \
         files when `file` names it.",
        json!({"plugin": {"type": "string"}, "name": {"type": "string"}, "file": {"type": "string"}}),
        &["plugin", "name"],
    )
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "input_schema": {"type": "object", "properties": properties, "required": required},
    })
}

const KNOWN: [&str; 10] = [
    "read_tool_output",
    "exec",
    "exec_poll",
    "exec_write",
    "exec_kill",
    "view_image",
    "fs_upload",
    "fs_download",
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
            "SELECT call_id,name,arguments,status,action_id,input FROM tool_calls
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
        let plugin = crate::plugins::run_tools(app, &mut tx, lease.org_id, run)
            .await?
            .into_iter()
            .find(|tool| tool.name == name);
        if !KNOWN.contains(&name.as_str())
            && !crate::memory::TOOLS.contains(&name.as_str())
            && name != "load_skill"
            && plugin.is_none()
        {
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
        // A plugin call waiting for the member to answer its server's question.
        let input: Option<Value> = row.get("input");
        if let Some(input) = &input
            && input.get("answer").is_none()
        {
            let decision: Option<String> =
                sqlx::query_scalar("SELECT status FROM pending_actions WHERE org_id=$1 AND id=$2")
                    .bind(lease.org_id)
                    .bind(row.get::<Option<Uuid>, _>("action_id"))
                    .fetch_optional(&mut *tx)
                    .await?;
            tx.commit().await?;
            if decision.as_deref() == Some("pending") {
                return Ok(false);
            }
            record(
                app,
                lease,
                run,
                &call,
                json!({"error":"input_not_given"}),
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
        let outcome = match plugin {
            Some(tool) => match plugin_call(app, lease, run, &call, &tool, args, input).await {
                Ok(Some(result)) => Ok(result),
                // Its server asked the member something: the run waits for the answer.
                Ok(None) => {
                    link.committed().await;
                    return Ok(false);
                }
                Err(error) => Err(error),
            },
            None => dispatch(app, lease, run, &call, &name, args, link).await,
        };
        match outcome {
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
                    | Code::PolicyDenied
                    | Code::Forbidden
                    | Code::InvalidArguments),
                ) => record(app, lease, run, &call, json!({"error":code.as_str()}), link).await?,
                _ => return Err(error),
            },
        }
    }
}

/// Call a plugin tool for the Chat loop. Chat asks no approval; the plugin's own rules
/// already removed tools it denies. `input` holds a question its server asked earlier, with
/// the member's answer. None: the server asks a question now, and the run waits.
async fn plugin_call(
    app: &App,
    lease: &Lease,
    run: Uuid,
    call: &str,
    tool: &crate::plugins::RunTool,
    args: Value,
    input: Option<Value>,
) -> anyhow::Result<Option<Value>> {
    use crate::plugins::elicit;
    let answers = input.as_ref().map(|input| {
        (
            elicit::responses(
                &input["requests"],
                input["key"].as_str().unwrap_or_default(),
                input["answer"].as_str().unwrap_or("decline"),
            )
            .unwrap_or_else(|_| json!({})),
            input["state"].as_str().map(str::to_owned),
        )
    });
    match crate::plugins::call(app, lease.org_id, tool, args, answers).await {
        Ok(crate::plugins::client::Reply::Done(result)) => Ok(Some(result)),
        Ok(crate::plugins::client::Reply::InputRequired { requests, state }) => {
            let Some((key, form)) = elicit::question(&requests) else {
                return Ok(Some(json!({"error": "unsupported_input_request"})));
            };
            let summary: String = format!(
                "{}：{}",
                tool.plugin,
                form["message"].as_str().unwrap_or("需要你提供信息")
            )
            .chars()
            .take(4000)
            .collect();
            let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
            let mut action = new_action(
                &mut tx,
                lease,
                run,
                "question",
                &summary,
                &["__answer__"],
                &app.config,
            )
            .await?;
            sqlx::query("UPDATE pending_actions SET form=$3 WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(action.id)
                .bind(&form)
                .execute(&mut *tx)
                .await?;
            action.form = Some(form);
            sqlx::query(
                "UPDATE tool_calls SET input=$4,action_id=$5 WHERE org_id=$1 AND run_id=$2 AND call_id=$3",
            )
            .bind(lease.org_id)
            .bind(run)
            .bind(call)
            .bind(json!({"requests": requests, "state": state, "key": key}))
            .bind(action.id)
            .execute(&mut *tx)
            .await?;
            request_action(&mut tx, lease, run, action).await?;
            tx.commit().await?;
            Ok(None)
        }
        // The server's own answer is the model's to read; ours are codes.
        Err(error) if is_fenced(&error) => Err(error),
        Err(error) => match error.downcast_ref::<crate::plugins::client::McpError>() {
            Some(crate::plugins::client::McpError(detail)) => Ok(Some(json!({"error": detail}))),
            None if error
                .downcast_ref::<crate::plugins::client::AuthRequired>()
                .is_some() =>
            {
                Ok(Some(json!({"error": "account_not_connected"})))
            }
            None => Ok(Some(
                json!({"error": crate::error::public_code(&error).as_str()}),
            )),
        },
    }
}

/// A page of a tool result of the user's conversations, by call id. Results reach a context
/// from the conversation's own runs and from those it was forked from, all the user's.
async fn read_tool_output(tx: &mut Tx<'_>, lease: &Lease, args: &Value) -> anyhow::Result<Value> {
    let id = args["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 256)
        .context(Code::InvalidToolArguments)?;
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let result: Option<Value> = sqlx::query_scalar(
        "SELECT t.result FROM tool_calls t
         JOIN runs r ON r.org_id=t.org_id AND r.id=t.run_id
         JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id
         WHERE t.org_id=$1 AND t.call_id=$2 AND t.status='completed'
           AND c.creator_id=(SELECT creator_id FROM conversations WHERE org_id=$1 AND id=$3)
         ORDER BY t.completed_at DESC LIMIT 1",
    )
    .bind(lease.org_id)
    .bind(id)
    .bind(lease.conversation_id)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    let Some(result) = result else {
        return Ok(json!({"error": "unknown_tool_output"}));
    };
    let text = result.to_string();
    let mut start = offset.min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + READ_PAGE_BYTES).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(json!({
        "id": id,
        "offset": start,
        "total_bytes": text.len(),
        "text": &text[start..end],
        "next_offset": (end < text.len()).then_some(end),
    }))
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
    if name == "load_skill" {
        let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
        let skills = crate::plugins::run_skills(&mut tx, lease.org_id, run).await?;
        tx.commit().await?;
        let (plugin, skill) = skills
            .into_iter()
            .find(|(plugin, skill)| {
                args["plugin"] == plugin.as_str() && args["name"] == skill.name.as_str()
            })
            .context(Code::InvalidToolArguments)?;
        return Ok(match args["file"].as_str().filter(|f| !f.is_empty()) {
            Some(file) => json!({
                "plugin": plugin, "skill": skill.name, "file": file,
                "text": skill.files.get(file).context(Code::InvalidToolArguments)?,
            }),
            None => json!({
                "plugin": plugin, "skill": skill.name, "text": skill.body,
                "files": skill.files.keys().collect::<Vec<_>>(),
            }),
        });
    }
    if name == "read_tool_output" {
        let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
        let result = read_tool_output(&mut tx, lease, &args).await;
        tx.commit().await?;
        return result;
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
                // The call shows its output as the command writes it.
                let live = if result["state"] == "running" {
                    Some(running(app, lease, run, call, link).await?)
                } else {
                    None
                };
                while result["state"] == "running" {
                    let tx = crate::runs::guarded(&app.store, lease, run).await?;
                    tx.commit().await?;
                    if let (Some(item), Some(output)) = (&live, result["output"].as_str()) {
                        link.send(WorkerEvent::Replace {
                            item: item.clone(),
                            stream: crate::owner::TERMINAL_OUTPUT,
                            text: output.into(),
                        })
                        .await?;
                    }
                    let poll = GuestCommand::Poll {
                        op_id: op.clone(),
                        wait_ms: OUTPUT_POLL_MS,
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
        "fs_upload" => {
            let name = args["name"].as_str().context(Code::InvalidToolArguments)?;
            let path = args["path"].as_str().context(Code::InvalidToolArguments)?;
            ensure!(
                !name.is_empty() && !name.contains(['/', '\0']) && !path.contains('\0'),
                Code::InvalidToolArguments
            );
            let destination = if path.starts_with('/') {
                path.to_owned()
            } else {
                format!("/workspace/{path}")
            };
            let (source, destination) =
                (quote(&format!("/attachments/{name}")), quote(&destination));
            let cmd = format!(
                "mkdir -p -- \"$(dirname -- {destination})\" && cp -- {source} {destination} \
                 && ls -l -- {destination}"
            );
            let spec: ExecSpec = serde_json::from_value(json!({"cmd": cmd}))?;
            let command = GuestCommand::Exec {
                op_id: op.clone(),
                spec,
            };
            let mut result = sandbox::guest(app, lease, sandbox, command).await?;
            while result["state"] == "running" {
                let poll = GuestCommand::Poll {
                    op_id: op.clone(),
                    wait_ms: EXEC_POLL_MS,
                };
                result = sandbox::guest(app, lease, sandbox, poll).await?;
            }
            sandbox::sync_events(app, lease, run, sandbox, link).await?;
            result
        }
        "fs_download" => {
            let path = args["path"].as_str().context(Code::InvalidToolArguments)?;
            let relative = path
                .strip_prefix("/workspace/")
                .unwrap_or(path)
                .trim_start_matches('/')
                .to_owned();
            ensure!(!relative.is_empty(), Code::InvalidToolArguments);
            let link = format!("/workspace/{relative}");
            let target = crate::attachments::outputs::Target::Workspace(relative);
            let shared =
                crate::attachments::outputs::publish(app, lease, Some(sandbox), &[(link, target)])
                    .await;
            match shared.into_iter().next() {
                Some(shared) => match shared.file {
                    Some(file) => json!({
                        "path": format!("/attachments/{}", file.path),
                        "name": file.name,
                        "size": file.size,
                        "kind": file.kind,
                    }),
                    None => json!({"error": shared.error}),
                },
                None => json!({"error": "FILE_UNAVAILABLE"}),
            }
        }
        _ => anyhow::bail!(Code::InvalidToolArguments),
    };
    Ok(if value["state"] == "outcome_unknown" {
        json!({"error":"outcome_unknown","details":value})
    } else {
        value
    })
}

/// `text` as one single-quoted shell word.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
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

/// Marks a call's item as running: the revision its output streams on.
async fn running(
    app: &App,
    lease: &Lease,
    run: Uuid,
    call: &str,
    link: &OwnerLink,
) -> anyhow::Result<LiveRef> {
    let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
    let id: Uuid = sqlx::query_scalar(
        "SELECT item_id FROM tool_calls WHERE org_id=$1 AND run_id=$2 AND call_id=$3",
    )
    .bind(lease.org_id)
    .bind(run)
    .bind(call)
    .fetch_one(&mut *tx)
    .await?;
    let mut item = load_item(&mut tx, lease.org_id, id).await?;
    if item.state != "running" {
        item.revision += 1;
        item.state = "running".into();
        item = upsert_item(&mut tx, lease, item).await?;
    }
    tx.commit().await?;
    link.committed().await;
    Ok(LiveRef::of(&item))
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
    // A plugin's question: its call goes on with the answer.
    let waiting: Option<String> = sqlx::query_scalar(
        "UPDATE tool_calls SET input=input||jsonb_build_object('answer',$3::text)
         WHERE org_id=$1 AND action_id=$2 AND input IS NOT NULL RETURNING call_id",
    )
    .bind(lease.org_id)
    .bind(action)
    .bind(answer)
    .fetch_optional(&mut **tx)
    .await?;
    if waiting.is_some() {
        return Ok(());
    }
    // A Work plugin's question, which the gateway waits on.
    let gateway: Option<Uuid> = sqlx::query_scalar(
        "UPDATE mcp_calls SET input=input||jsonb_build_object('answer',$3::text),state='completed'
         WHERE org_id=$1 AND action_id=$2 AND input IS NOT NULL RETURNING id",
    )
    .bind(lease.org_id)
    .bind(action)
    .bind(answer)
    .fetch_optional(&mut **tx)
    .await?;
    if gateway.is_some() {
        return Ok(());
    }
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
