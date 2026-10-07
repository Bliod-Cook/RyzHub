//! Native harness processes and their protocol adapter.
//!
//! Codex app-server and the Claude Agent SDK adapter (`harness/claude`) both speak the Codex
//! JSON-RPC dialect over stdio. This module is the only place that knows that dialect: it
//! turns [`HarnessRequest`]s into native calls and native output into [`HarnessEvent`]s.
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::ChildStdin,
    sync::Mutex,
};

use crate::{
    guest::Guest,
    harness::{
        ApprovalCategory, FileChange, HarnessAnswer, HarnessEvent, HarnessRequest, HarnessResult,
        ItemContent, ItemKind, ItemPhase, ModelInfo, Question, TurnSettings, TurnStatus,
    },
};

/// Harness versions the platform is tested against. Checkpoints record the version they were
/// taken with; a restore requires the same version.
pub const CODEX_VERSION: &str = "0.154.0";
pub const CLAUDE_SDK_VERSION: &str = "0.3.283";
/// Native lines above this are rejected; the harness is restarted.
const MAX_LINE: usize = 4 * 1024 * 1024;
/// MCP calls may wait for a user approval; the harness must not time them out first.
const MCP_TOOL_TIMEOUT_SECS: u64 = 25 * 3600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Codex,
    Claude,
}

impl Harness {
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        match name {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            other => anyhow::bail!("unsupported harness {other}"),
        }
    }
}

pub fn harness_version(harness: &str) -> &'static str {
    if harness == "claude" {
        CLAUDE_SDK_VERSION
    } else {
        CODEX_VERSION
    }
}

/// What each harness supports natively, as reported to clients.
pub fn capabilities(harness: &str) -> Value {
    if harness == "claude" {
        return json!({
            "harness": "claude",
            "protocol": "agent-sdk",
            "tested_version": CLAUDE_SDK_VERSION,
            "steer": "emulated",
            "interrupt": "native",
            "resume": "native",
            "approval": "native",
            "question": "native",
            "fork": "native",
            "compact": "native",
            "pty": false,
        });
    }
    json!({
        "harness": "codex",
        "protocol": "app-server",
        "tested_version": CODEX_VERSION,
        "send": "native",
        "steer": "native",
        "interrupt": "native",
        "resume": "native",
        "approval": "native",
        "question": "native",
        "side_query": "unsupported",
        "fork": "unsupported",
        "pty": false,
    })
}

pub struct Native {
    input: Mutex<ChildStdin>,
    harness: Harness,
}

impl Native {
    pub async fn start(
        guest: Arc<Guest>,
        model: &str,
        mcp: bool,
        harness: &str,
    ) -> anyhow::Result<Arc<Self>> {
        ensure!(!model.is_empty() && model.len() < 256, "invalid model");
        let harness = Harness::parse(harness)?;
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/agent".into());
        let config_dir = std::path::Path::new(&home).join(".codex");
        std::fs::create_dir_all(&config_dir)?;
        // The harness talks only to the guest's local proxy, which adds the gateway key.
        let mut config = format!(
            r#"model = {}
model_provider = "ryzhub"
web_search = "live"
[model_providers.ryzhub]
name = "RyzHub"
base_url = "http://127.0.0.1:8090/v1"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
request_max_retries = 0
stream_max_retries = 0
"#,
            serde_json::to_string(model)?
        );
        if mcp {
            config.push_str(&format!(
                "[mcp_servers.ryzhub]\nurl = \"http://127.0.0.1:8090/mcp\"\ntool_timeout_sec = {MCP_TOOL_TIMEOUT_SECS}\n"
            ));
        }
        // No provider or gateway credential ever enters the durable home directory.
        crate::journal::atomic_bytes(&config_dir.join("config.toml"), config.as_bytes())?;
        let mut command = match harness {
            Harness::Claude => {
                let mut command = tokio::process::Command::new("/usr/local/bin/node");
                command.arg("/opt/ryzhub-claude/dist/adapter.js");
                command
            }
            Harness::Codex => {
                let mut command = tokio::process::Command::new("/codex");
                command.arg("app-server");
                command
            }
        };
        let mut child = command
            .env_clear()
            .env("HOME", &home)
            .env("CODEX_HOME", &config_dir)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("TERM", "dumb")
            .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:8090")
            .env("ANTHROPIC_API_KEY", "guest-proxy")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env(
                "MCP_TOOL_TIMEOUT",
                (MCP_TOOL_TIMEOUT_SECS * 1000).to_string(),
            )
            .env("RYZHUB_CLAUDE_MODEL", model)
            .env("RYZHUB_MCP", if mcp { "1" } else { "0" })
            .env("TOKIO_WORKER_THREADS", "2")
            .current_dir("/workspace")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let native = Arc::new(Self {
            input: Mutex::new(child.stdin.take().context("harness stdin")?),
            harness,
        });
        let mut lines = BufReader::new(child.stdout.take().context("harness stdout")?).lines();
        let this = native.clone();
        tokio::spawn(async move {
            let result = async {
                while let Some(line) = lines.next_line().await? {
                    ensure!(line.len() <= MAX_LINE, "native line too large");
                    let value: Value = serde_json::from_str(&line)?;
                    if value["id"] == "__initialize" && value.get("error").is_none() {
                        this.send(json!({"method":"initialized"})).await?;
                    }
                    match classify(&value) {
                        Incoming::AutoReply { id, result } => {
                            this.send(json!({"id":id,"result":result})).await?
                        }
                        incoming => guest.native_line(incoming).await?,
                    }
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            let status = match child.try_wait() {
                Ok(Some(status)) => Some(status),
                _ => {
                    let _ = child.kill().await;
                    child.wait().await.ok()
                }
            };
            let exited = HarnessEvent::Exited {
                exit_code: status.and_then(|s| s.code()),
                error: result.err().map(|e| e.to_string()),
            };
            let _ = guest.native_line(Incoming::Event(exited)).await;
        });
        native
            .send(json!({
                "id": "__initialize",
                "method": "initialize",
                "params": {
                    "clientInfo": {"name": "ryzhub", "version": "0.2.0"},
                    "capabilities": {"experimentalApi": true},
                },
            }))
            .await?;
        Ok(native)
    }

    pub async fn send(&self, value: Value) -> anyhow::Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        ensure!(bytes.len() <= MAX_LINE, "native request too large");
        bytes.push(b'\n');
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.input.lock().await.write_all(&bytes),
        )
        .await??;
        Ok(())
    }

    /// The native call for `request`, or None when this harness needs no call for it.
    pub fn request(&self, op_id: &str, request: &HarnessRequest) -> Option<Value> {
        let (method, params) = translate(self.harness, request)?;
        Some(json!({"id":op_id,"method":method,"params":params}))
    }

    /// The native reply to a harness approval or question.
    pub fn answer(request_id: &str, answer: &HarnessAnswer) -> anyhow::Result<Value> {
        let id: Value = serde_json::from_str(request_id).context("request id")?;
        let result = match answer {
            HarnessAnswer::Approval { allow } => {
                json!({"decision": if *allow { "accept" } else { "decline" }})
            }
            HarnessAnswer::Question { answers } => json!({
                "answers": answers
                    .iter()
                    .map(|(id, text)| (id.clone(), json!({"answers":[text]})))
                    .collect::<serde_json::Map<_, _>>()
            }),
        };
        Ok(json!({"id":id,"result":result}))
    }
}

fn session_params(harness: Harness, settings: &TurnSettings, instructions: &str) -> Value {
    let mut params = json!({
        "model": settings.model,
        "modelProvider": "ryzhub",
        "cwd": "/workspace",
        "approvalPolicy": approval_policy(settings),
        "sandbox": "danger-full-access",
        "persistExtendedHistory": true,
        "developerInstructions": instructions,
    });
    // The Claude adapter keeps session settings; Codex receives them per turn.
    if harness == Harness::Claude {
        params["config"] = json!(settings);
    }
    params
}

fn approval_policy(settings: &TurnSettings) -> &'static str {
    if settings.approval == "full_auto" {
        "never"
    } else {
        "untrusted"
    }
}

/// A user turn's input: the text, then each image as a local file the harness reads.
fn user_input(text: &str, images: &[String]) -> Value {
    let mut input = vec![json!({"type":"text","text":text,"text_elements":[]})];
    input.extend(
        images
            .iter()
            .map(|path| json!({"type":"localImage","path":path})),
    );
    Value::Array(input)
}

fn translate(harness: Harness, request: &HarnessRequest) -> Option<(&'static str, Value)> {
    Some(match request {
        HarnessRequest::StartThread {
            settings,
            instructions,
        } => (
            "thread/start",
            session_params(harness, settings, instructions),
        ),
        HarnessRequest::ResumeThread {
            thread_id,
            settings,
            instructions,
        } => {
            let mut params = session_params(harness, settings, instructions);
            params["threadId"] = json!(thread_id);
            ("thread/resume", params)
        }
        HarnessRequest::ForkThread {
            thread_id,
            last_turn_id,
            message_id,
            settings,
            instructions,
        } => {
            let mut params = session_params(harness, settings, instructions);
            params["threadId"] = json!(thread_id);
            params["lastTurnId"] = json!(last_turn_id);
            params["resumeSessionAt"] = json!(message_id);
            ("thread/fork", params)
        }
        HarnessRequest::Configure {
            settings,
            instructions,
        } => {
            if harness != Harness::Claude {
                return None;
            }
            let mut params = json!(settings);
            params["project"] = json!(instructions);
            ("session/configure", params)
        }
        HarnessRequest::StartTurn {
            thread_id,
            text,
            settings,
            images,
        } => {
            let mut params = json!({"threadId":thread_id,"input":user_input(text, images)});
            if harness == Harness::Codex {
                params["model"] = json!(settings.model);
                params["effort"] = json!(settings.reasoning);
                params["approvalPolicy"] = json!(approval_policy(settings));
                // Omitting the mode would keep the previous turn's mode.
                params["collaborationMode"] = json!({
                    "mode": if settings.plan { "plan" } else { "default" },
                    "settings": {"model":settings.model,"reasoning_effort":settings.reasoning,"developer_instructions":null},
                });
            }
            ("turn/start", params)
        }
        HarnessRequest::SteerTurn {
            thread_id,
            turn_id,
            text,
            images,
        } => (
            "turn/steer",
            json!({"threadId":thread_id,"expectedTurnId":turn_id,"input":user_input(text, images)}),
        ),
        HarnessRequest::InterruptTurn { thread_id, turn_id } => (
            "turn/interrupt",
            json!({"threadId":thread_id,"turnId":turn_id}),
        ),
        HarnessRequest::Compact { thread_id } => {
            ("thread/compact/start", json!({"threadId":thread_id}))
        }
        HarnessRequest::ListModels => ("model/list", json!({})),
    })
}

/// Normalize the native response to `request`.
pub fn result_of(request: &HarnessRequest, response: &Value) -> Result<HarnessResult, String> {
    if let Some(error) = response.get("error") {
        return Err(error["message"]
            .as_str()
            .map_or_else(|| error.to_string(), str::to_owned));
    }
    let result = &response["result"];
    Ok(match request {
        HarnessRequest::StartThread { .. }
        | HarnessRequest::ResumeThread { .. }
        | HarnessRequest::ForkThread { .. } => HarnessResult {
            thread_id: result["thread"]["id"].as_str().map(str::to_owned),
            ..Default::default()
        },
        HarnessRequest::StartTurn { .. } | HarnessRequest::Compact { .. } => HarnessResult {
            turn_id: result["turn"]["id"].as_str().map(str::to_owned),
            ..Default::default()
        },
        HarnessRequest::ListModels => HarnessResult {
            models: models(&result["data"]),
            ..Default::default()
        },
        _ => HarnessResult::default(),
    })
}

fn models(list: &Value) -> Vec<ModelInfo> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model["model"].as_str().or_else(|| model["id"].as_str())?;
            let reasoning_levels = model["supportedReasoningEfforts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|level| {
                    level["reasoningEffort"]
                        .as_str()
                        .or_else(|| level.as_str())
                        .map(str::to_owned)
                })
                .collect();
            Some(ModelInfo {
                id: id.into(),
                reasoning_levels,
            })
        })
        .collect()
}

/// What the guest does with one native line.
pub enum Incoming {
    /// The reply to a request the guest sent, keyed by its id.
    Response {
        id: String,
        response: Value,
    },
    /// A durable event to journal.
    Event(HarnessEvent),
    /// Streaming text of an item; folded, never journaled.
    Delta {
        item_id: String,
        text: String,
    },
    /// A harness request the platform does not support: answered at once, never granted.
    AutoReply {
        id: Value,
        result: Value,
    },
    Ignore,
}

pub fn classify(value: &Value) -> Incoming {
    let method = value["method"].as_str();
    let params = &value["params"];
    let turn = || {
        params["turnId"]
            .as_str()
            .or_else(|| params["turn"]["id"].as_str())
            .map(str::to_owned)
    };
    match (method, value.get("id")) {
        (None, Some(id)) => Incoming::Response {
            id: id.as_str().map_or_else(|| id.to_string(), str::to_owned),
            response: value.clone(),
        },
        (Some(method), Some(id)) => match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                Incoming::Event(HarnessEvent::ApprovalRequested {
                    request_id: id.to_string(),
                    turn_id: turn(),
                    category: if method.starts_with("item/fileChange") {
                        ApprovalCategory::FileChange
                    } else {
                        ApprovalCategory::Command
                    },
                    summary: format!(
                        "{}\n{}",
                        params["command"].as_str().unwrap_or("文件变更"),
                        params["reason"].as_str().unwrap_or("")
                    ),
                })
            }
            "item/tool/requestUserInput" => Incoming::Event(HarnessEvent::QuestionRequested {
                request_id: id.to_string(),
                turn_id: turn(),
                questions: params["questions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|q| Question {
                        id: q["id"].as_str().unwrap_or_default().into(),
                        question: q["question"].as_str().unwrap_or_default().into(),
                        options: q["options"].clone(),
                    })
                    .collect(),
            }),
            // Permission escalation is never granted automatically.
            "item/permissions/requestApproval" => Incoming::AutoReply {
                id: id.clone(),
                result: json!({"permissions":{},"scope":"turn"}),
            },
            _ => Incoming::AutoReply {
                id: id.clone(),
                result: json!({"decision":"decline"}),
            },
        },
        (Some(method), None) => {
            if method.ends_with("/delta") || method.ends_with("Delta") {
                // Only assistant text streams into the timeline; other deltas (command output,
                // reasoning summaries) arrive complete with their item.
                if method != "item/agentMessage/delta" {
                    return Incoming::Ignore;
                }
                return match (params["itemId"].as_str(), params["delta"].as_str()) {
                    (Some(item), Some(text)) => Incoming::Delta {
                        item_id: item.into(),
                        text: text.into(),
                    },
                    _ => Incoming::Ignore,
                };
            }
            match method {
                "turn/started" => match turn() {
                    Some(turn_id) => Incoming::Event(HarnessEvent::TurnStarted { turn_id }),
                    None => Incoming::Ignore,
                },
                "turn/completed" => Incoming::Event(HarnessEvent::TurnCompleted {
                    turn_id: turn(),
                    status: match params["turn"]["status"].as_str() {
                        Some("completed") => TurnStatus::Completed,
                        Some("interrupted") => TurnStatus::Interrupted,
                        _ => TurnStatus::Failed,
                    },
                }),
                "item/started" | "item/completed" => item_event(method, params, turn()),
                "session/capabilities" => Incoming::Event(HarnessEvent::Capabilities {
                    commands: params["commands"].clone(),
                    models: models(&params["models"]),
                }),
                _ => Incoming::Ignore,
            }
        }
        (None, None) => Incoming::Ignore,
    }
}

fn item_event(method: &str, params: &Value, turn_id: Option<String>) -> Incoming {
    let item = &params["item"];
    let kind = item["type"].as_str().unwrap_or("unknown");
    // The platform already committed the user's input before calling the harness.
    if kind == "userMessage" {
        return Incoming::Ignore;
    }
    let Some(native_id) = item["id"].as_str() else {
        return Incoming::Ignore;
    };
    let text = |key: &str| item[key].as_str().unwrap_or_default().to_owned();
    let (kind, content) = match kind {
        "agentMessage" => (
            ItemKind::Assistant,
            ItemContent {
                text: text("text"),
                ..Default::default()
            },
        ),
        "plan" => (
            ItemKind::Plan,
            ItemContent {
                text: text("text"),
                ..Default::default()
            },
        ),
        "reasoning" => (
            ItemKind::Reasoning,
            ItemContent {
                text: item["summary"]
                    .as_array()
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default(),
                ..Default::default()
            },
        ),
        "commandExecution" => (
            ItemKind::Command,
            ItemContent {
                text: format!("{}\n{}", text("command"), text("aggregatedOutput")),
                name: Some(text("command")),
                output: Some(text("aggregatedOutput")),
                ..Default::default()
            },
        ),
        "fileChange" => (
            ItemKind::FileChange,
            ItemContent {
                text: item["changes"].to_string(),
                file_changes: file_changes(&item["changes"]),
                ..Default::default()
            },
        ),
        "mcpToolCall" => (
            ItemKind::ToolCall,
            ItemContent {
                text: format!("MCP {} / {}", item["server"], item["tool"]),
                name: Some(format!(
                    "{}/{}",
                    item["server"].as_str().unwrap_or_default(),
                    item["tool"].as_str().unwrap_or_default()
                )),
                arguments_json: Some(item["arguments"].to_string()),
                result_json: item.get("result").map(Value::to_string),
                ..Default::default()
            },
        ),
        // The provider's own search, also emitted by the Claude adapter for its WebSearch tool.
        "webSearch" => {
            let action = &item["action"];
            let arguments = match [&item["query"], &action["query"]]
                .into_iter()
                .find_map(|q| q.as_str().filter(|q| !q.is_empty()))
            {
                Some(query) => json!({ "query": query }),
                None => json!({ "url": action["url"].as_str().unwrap_or_default() }),
            };
            (
                ItemKind::ToolCall,
                ItemContent {
                    text: format!("web_search {arguments}"),
                    name: Some("web_search".into()),
                    arguments_json: Some(arguments.to_string()),
                    output: item["output"].as_str().map(str::to_owned),
                    ..Default::default()
                },
            )
        }
        other => (
            ItemKind::Other,
            ItemContent {
                text: format!("Codex: {other}"),
                ..Default::default()
            },
        ),
    };
    Incoming::Event(HarnessEvent::Item {
        phase: if method == "item/completed" {
            ItemPhase::Completed
        } else {
            ItemPhase::Started
        },
        turn_id,
        native_id: native_id.into(),
        kind,
        content,
        message_id: item["nativeMessageId"].as_str().map(str::to_owned),
    })
}

fn file_changes(changes: &Value) -> Vec<FileChange> {
    let entry = |path: &str, change: &Value| FileChange {
        path: path.into(),
        change: change["kind"]["type"]
            .as_str()
            .or_else(|| change["kind"].as_str())
            .or_else(|| change["type"].as_str())
            .unwrap_or("update")
            .into(),
        diff: change["diff"]
            .as_str()
            .or_else(|| change["unified_diff"].as_str())
            .unwrap_or_default()
            .into(),
    };
    match changes {
        Value::Array(list) => list
            .iter()
            .map(|change| entry(change["path"].as_str().unwrap_or_default(), change))
            .collect(),
        Value::Object(map) => map
            .iter()
            .map(|(path, change)| entry(path, change))
            .collect(),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_dialect_maps_to_platform_events_and_back() {
        let approval = json!({"id":7,"method":"item/commandExecution/requestApproval","params":{"turnId":"t","command":"rm -rf x","reason":"cleanup"}});
        let Incoming::Event(HarnessEvent::ApprovalRequested {
            request_id,
            category,
            summary,
            ..
        }) = classify(&approval)
        else {
            panic!("approval not classified")
        };
        assert_eq!(
            (request_id.as_str(), category),
            ("7", ApprovalCategory::Command)
        );
        assert_eq!(summary, "rm -rf x\ncleanup");
        let answer = Native::answer(&request_id, &HarnessAnswer::Approval { allow: true }).unwrap();
        assert_eq!(answer, json!({"id":7,"result":{"decision":"accept"}}));
        let unknown = json!({"id":"x","method":"item/network/request","params":{}});
        assert!(matches!(classify(&unknown), Incoming::AutoReply { .. }));
        let command = json!({"method":"item/completed","params":{"turn":{"id":"t"},"item":{"id":"i","type":"commandExecution","command":"ls","aggregatedOutput":"a\n"}}});
        let Incoming::Event(HarnessEvent::Item {
            kind,
            content,
            turn_id,
            ..
        }) = classify(&command)
        else {
            panic!("item not classified")
        };
        assert_eq!((kind, turn_id.as_deref()), (ItemKind::Command, Some("t")));
        assert_eq!(content.text, "ls\na\n");
        for (item, arguments) in [
            (
                json!({"id":"w","type":"webSearch","query":"rust 2026"}),
                json!({"query":"rust 2026"}),
            ),
            (
                json!({"id":"w","type":"webSearch","query":"","action":{"type":"openPage","url":"https://a.example"}}),
                json!({"url":"https://a.example"}),
            ),
        ] {
            let search =
                json!({"method":"item/completed","params":{"turn":{"id":"t"},"item":item}});
            let Incoming::Event(HarnessEvent::Item { kind, content, .. }) = classify(&search)
            else {
                panic!("search not classified")
            };
            assert_eq!(kind, ItemKind::ToolCall);
            assert_eq!(content.name.as_deref(), Some("web_search"));
            assert_eq!(content.arguments_json, Some(arguments.to_string()));
        }
        let models = result_of(
            &HarnessRequest::ListModels,
            &json!({"result":{"data":[{"model":"gpt","supportedReasoningEfforts":[{"reasoningEffort":"low"}]}]}}),
        )
        .unwrap();
        assert_eq!(models.models[0].reasoning_levels, ["low"]);
        let turn = translate(
            Harness::Codex,
            &HarnessRequest::StartTurn {
                thread_id: "th".into(),
                text: "hi".into(),
                settings: TurnSettings {
                    plan: true,
                    ..Default::default()
                },
                images: vec![],
            },
        )
        .unwrap();
        assert_eq!(turn.1["collaborationMode"]["mode"], "plan");
        assert_eq!(turn.1["input"].as_array().unwrap().len(), 1);
        let steer = HarnessRequest::SteerTurn {
            thread_id: "th".into(),
            turn_id: "t1".into(),
            text: "look".into(),
            images: vec!["/attachments/a.png".into()],
        };
        let (_, params) = translate(Harness::Codex, &steer).unwrap();
        assert_eq!(
            params["input"][1],
            json!({"type":"localImage","path":"/attachments/a.png"})
        );
        // Requests without images serialize as before, so journal fingerprints hold.
        let plain = HarnessRequest::SteerTurn {
            thread_id: "th".into(),
            turn_id: "t1".into(),
            text: "look".into(),
            images: vec![],
        };
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            json!({"type":"steer_turn","thread_id":"th","turn_id":"t1","text":"look"})
        );
        assert!(
            translate(
                Harness::Codex,
                &HarnessRequest::Configure {
                    settings: TurnSettings::default(),
                    instructions: String::new()
                }
            )
            .is_none()
        );
    }
}
