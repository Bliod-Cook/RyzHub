//! Agents spoken to over the Agent Client Protocol (v1).
//!
//! The guest drives every harness in the Codex dialect (see `native`). [`Bridge`] stands in for
//! an ACP agent there: it turns [`HarnessRequest`]s into ACP calls, and the agent's messages
//! into the replies and events Codex would have sent. An ACP prompt is answered only when its
//! turn ends and updates name no turn, so the bridge names turns itself and answers a turn's
//! start at once. It does no IO: [`Out`] says what goes where.
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::{
    AcpProfile,
    harness::{
        ApprovalCategory, FileChange, HarnessAnswer, HarnessEvent, HarnessRequest, ItemContent,
        ItemKind, ItemPhase, TurnSettings, TurnStatus,
    },
    native::Incoming,
};

/// The protocol version the guest speaks.
pub const PROTOCOL_VERSION: u64 = 1;
/// The guest's MCP endpoint, offered to agents that take MCP servers over HTTP.
const MCP_URL: &str = "http://127.0.0.1:8090/mcp";
/// The mode a plan asks for.
const PLAN_MODE: &str = "plan";

/// Where a message goes.
#[derive(Debug)]
pub enum Out {
    /// To the agent, as a line on its stdin.
    Agent(Value),
    /// To the guest, as if the harness had sent it.
    Guest(Box<Incoming>),
}

/// What a call to the agent was for.
enum Call {
    Initialize,
    /// `session/new`, or `session/load` of `loaded`, for platform operation `op`.
    Session {
        op: String,
        loaded: Option<String>,
    },
    Prompt {
        turn: String,
    },
    /// A session option being set.
    Option,
}

/// Text the agent is writing: a reply or its reasoning.
struct Written {
    native_id: String,
    message_id: Option<String>,
    text: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Writing {
    Reply,
    Thought,
}

struct Turn {
    id: String,
    reply: Option<Written>,
    thought: Option<Written>,
    /// Items the agent does not name, numbered.
    count: u64,
}

/// A tool call as its updates have described it so far.
struct Tool {
    native_id: String,
    /// The tool's name: the agent's, else the call's first title.
    name: String,
    state: Value,
    /// Output already streamed.
    streamed: usize,
}

/// A session option: its current value and the values it takes.
#[derive(Clone, Debug, Default)]
struct Choice {
    current: String,
    values: Vec<String>,
    /// Set with `session/set_mode` (the older modes API) rather than as a config option.
    mode_api: bool,
}

pub struct Bridge {
    profile: AcpProfile,
    /// The models the harness may switch between.
    models: Vec<String>,
    /// Whether to offer the agent the platform's tools.
    mcp: bool,
    next: u64,
    calls: HashMap<String, Call>,
    /// The agent's capabilities, once it has answered `initialize`.
    agent: Option<Value>,
    /// Why the agent could not be initialized.
    broken: Option<String>,
    /// Platform requests that came before the agent answered `initialize`.
    waiting: Vec<(String, HarnessRequest, Vec<Value>)>,
    session: Option<String>,
    /// A loaded session's history is being replayed: it is not news.
    loading: bool,
    options: HashMap<String, Choice>,
    /// The mode the session works in when it is not planning.
    work_mode: Option<String>,
    settings: TurnSettings,
    /// Option changes in flight; the next prompt waits for them.
    setting: usize,
    /// A prompt waiting for option changes: its call id and message.
    deferred: Option<(String, Value)>,
    turn: Option<Turn>,
    tools: HashMap<String, Tool>,
    /// Permission requests the user decides, by the request id the guest knows: the JSON-RPC
    /// id and the options offered.
    permissions: HashMap<String, (Value, Vec<Value>)>,
    /// Agent messages ended in this turn, with how often: one that continues after a tool call
    /// becomes another item.
    ended: HashMap<String, u64>,
}

impl Bridge {
    pub fn new(profile: AcpProfile, models: Vec<String>, mcp: bool) -> Self {
        Self {
            profile,
            models,
            mcp,
            next: 0,
            calls: HashMap::new(),
            agent: None,
            broken: None,
            waiting: vec![],
            session: None,
            loading: false,
            options: HashMap::new(),
            work_mode: None,
            settings: TurnSettings::default(),
            setting: 0,
            deferred: None,
            turn: None,
            tools: HashMap::new(),
            permissions: HashMap::new(),
            ended: HashMap::new(),
        }
    }

    /// The first message to the agent.
    pub fn initialize(&mut self) -> Value {
        let id = self.call(Call::Initialize);
        message(
            &id,
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                // Files and commands stay the agent's own business, inside the sandbox.
                "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
                "clientInfo": {"name": "ryzhub", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
    }

    fn call(&mut self, call: Call) -> String {
        self.next += 1;
        let id = format!("ryzhub-{}", self.next);
        self.calls.insert(id.clone(), call);
        id
    }

    /// Platform operation `op`. `images` are the image content blocks of a turn's input.
    pub fn request(&mut self, op: &str, request: &HarnessRequest, images: Vec<Value>) -> Vec<Out> {
        let mut out = vec![];
        if let Some(error) = &self.broken {
            out.push(failure(op, error));
            return out;
        }
        let Some(agent) = self.agent.clone() else {
            self.waiting.push((op.into(), request.clone(), images));
            return out;
        };
        match request {
            HarnessRequest::StartThread { settings, .. } => {
                self.settings = settings.clone();
                let id = self.call(Call::Session {
                    op: op.into(),
                    loaded: None,
                });
                let params = json!({"cwd": "/workspace", "mcpServers": self.mcp_servers()});
                out.push(Out::Agent(message(&id, "session/new", params)));
            }
            HarnessRequest::ResumeThread {
                thread_id,
                settings,
                ..
            } => {
                self.settings = settings.clone();
                if self.session.as_deref() == Some(thread_id) {
                    out.push(reply(op, json!({"thread": {"id": thread_id}})));
                    self.apply(&mut out);
                } else if agent["loadSession"] == true {
                    self.loading = true;
                    let id = self.call(Call::Session {
                        op: op.into(),
                        loaded: Some(thread_id.clone()),
                    });
                    let params = json!({"sessionId": thread_id, "cwd": "/workspace", "mcpServers": self.mcp_servers()});
                    out.push(Out::Agent(message(&id, "session/load", params)));
                } else {
                    out.push(failure(op, "the agent cannot resume sessions"));
                }
            }
            HarnessRequest::ForkThread { .. } => {
                out.push(failure(op, "the agent cannot fork sessions"));
            }
            HarnessRequest::Configure { settings, .. } => {
                self.settings = settings.clone();
                self.apply(&mut out);
                out.push(reply(op, json!({})));
            }
            HarnessRequest::StartTurn {
                thread_id,
                text,
                settings,
                ..
            } => {
                if self.turn.is_some() {
                    out.push(failure(op, "a turn is in progress"));
                    return out;
                }
                if self.session.as_deref() != Some(thread_id) {
                    out.push(failure(op, "no such session"));
                    return out;
                }
                self.settings = settings.clone();
                self.apply(&mut out);
                let turn = format!("acp-{}", uuid::Uuid::now_v7().simple());
                let mut prompt = vec![json!({"type": "text", "text": text})];
                // The input lists every file by path; agents that take no images read them
                // from there.
                if agent["promptCapabilities"]["image"] == true {
                    prompt.extend(images);
                }
                let id = self.call(Call::Prompt { turn: turn.clone() });
                let params = json!({"sessionId": thread_id, "prompt": prompt});
                let prompt = message(&id, "session/prompt", params);
                if self.setting > 0 {
                    self.deferred = Some((id, prompt));
                } else {
                    out.push(Out::Agent(prompt));
                }
                self.turn = Some(Turn {
                    id: turn.clone(),
                    reply: None,
                    thought: None,
                    count: 0,
                });
                out.push(reply(op, json!({"turn": {"id": turn}})));
                out.push(event(HarnessEvent::TurnStarted { turn_id: turn }));
            }
            HarnessRequest::SteerTurn { .. } => {
                out.push(failure(op, "the agent cannot steer turns"));
            }
            HarnessRequest::InterruptTurn { .. } => {
                self.cancel(&mut out);
                out.push(reply(op, json!({})));
            }
            HarnessRequest::Compact { .. } => {
                out.push(failure(op, "the agent compacts in a turn"));
            }
            HarnessRequest::ListModels => {
                let data: Vec<Value> = self.models.iter().map(|m| json!({"id": m})).collect();
                out.push(reply(op, json!({"data": data})));
            }
        }
        out
    }

    /// The user's decision on a permission request.
    pub fn answer(&mut self, request_id: &str, answer: &HarnessAnswer) -> Vec<Out> {
        let Some((id, options)) = self.permissions.remove(request_id) else {
            return vec![];
        };
        let allow = matches!(answer, HarnessAnswer::Approval { allow: true });
        let kinds: &[&str] = if allow {
            &["allow_once", "allow_always"]
        } else {
            &["reject_once", "reject_always"]
        };
        vec![Out::Agent(choose(&id, &options, kinds))]
    }

    /// One message from the agent.
    pub fn incoming(&mut self, message: &Value) -> Vec<Out> {
        let mut out = vec![];
        match (message["method"].as_str(), message.get("id")) {
            (None, Some(id)) => self.response(id, message, &mut out),
            (Some(method), Some(id)) => {
                self.agent_request(method, id, &message["params"], &mut out)
            }
            (Some("session/update"), None) => self.update(&message["params"], &mut out),
            _ => {}
        }
        out
    }

    fn mcp_servers(&self) -> Value {
        let http = self
            .agent
            .as_ref()
            .is_some_and(|agent| agent["mcpCapabilities"]["http"] == true);
        if self.mcp && http {
            json!([{"type": "http", "name": "ryzhub", "url": MCP_URL, "headers": []}])
        } else {
            json!([])
        }
    }

    fn response(&mut self, id: &Value, message: &Value, out: &mut Vec<Out>) {
        let key = id.as_str().map_or_else(|| id.to_string(), str::to_owned);
        let Some(call) = self.calls.remove(&key) else {
            return;
        };
        let error = message.get("error").map(|error| {
            error["message"]
                .as_str()
                .map_or_else(|| error.to_string(), str::to_owned)
        });
        let result = &message["result"];
        match call {
            Call::Initialize => match error {
                Some(error) => {
                    for (op, ..) in std::mem::take(&mut self.waiting) {
                        out.push(failure(&op, &error));
                    }
                    self.broken = Some(error);
                }
                None => {
                    self.agent = Some(result["agentCapabilities"].clone());
                    for (op, request, images) in std::mem::take(&mut self.waiting) {
                        out.extend(self.request(&op, &request, images));
                    }
                }
            },
            Call::Session { op, loaded } => {
                self.loading = false;
                let session = loaded.or_else(|| result["sessionId"].as_str().map(str::to_owned));
                match (error, session) {
                    (Some(error), _) => out.push(failure(&op, &error)),
                    (None, None) => out.push(failure(&op, "the agent named no session")),
                    (None, Some(session)) => {
                        self.session = Some(session.clone());
                        self.read_options(result);
                        out.push(reply(&op, json!({"thread": {"id": session}})));
                        self.apply(out);
                    }
                }
            }
            Call::Prompt { turn } => {
                let status = match (&error, result["stopReason"].as_str()) {
                    (Some(_), _) | (_, Some("refusal")) => TurnStatus::Failed,
                    (_, Some("cancelled")) => TurnStatus::Interrupted,
                    _ => TurnStatus::Completed,
                };
                self.finish_turn(&turn, status, out);
            }
            Call::Option => {
                self.setting = self.setting.saturating_sub(1);
                if error.is_none() {
                    self.read_options(result);
                }
                if self.setting == 0
                    && let Some((_, prompt)) = self.deferred.take()
                {
                    out.push(Out::Agent(prompt));
                }
            }
        }
    }

    /// The session's options, from a response or update that lists them.
    fn read_options(&mut self, value: &Value) {
        for option in value["configOptions"].as_array().into_iter().flatten() {
            let (Some(id), Some(current)) =
                (option["id"].as_str(), option["currentValue"].as_str())
            else {
                continue;
            };
            // Values come flat or in groups.
            let mut values = vec![];
            for entry in option["options"].as_array().into_iter().flatten() {
                match entry["value"].as_str() {
                    Some(value) => values.push(value.to_owned()),
                    None => values.extend(
                        entry["options"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|o| o["value"].as_str().map(str::to_owned)),
                    ),
                }
            }
            self.options.insert(
                id.into(),
                Choice {
                    current: current.into(),
                    values,
                    mode_api: false,
                },
            );
        }
        let modes = &value["modes"];
        if let Some(current) = modes["currentModeId"].as_str()
            && !self.options.get("mode").is_some_and(|m| !m.mode_api)
        {
            let values = modes["availableModes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["id"].as_str().map(str::to_owned))
                .collect();
            self.options.insert(
                "mode".into(),
                Choice {
                    current: current.into(),
                    values,
                    mode_api: true,
                },
            );
        }
        if self.work_mode.is_none()
            && let Some(mode) = self.options.get("mode")
        {
            self.work_mode = Some(if mode.current != PLAN_MODE {
                mode.current.clone()
            } else {
                mode.values
                    .iter()
                    .find(|v| *v != PLAN_MODE)
                    .cloned()
                    .unwrap_or_default()
            });
        }
    }

    /// Bring the session's mode and model in line with the settings.
    fn apply(&mut self, out: &mut Vec<Out>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let mut changes = vec![];
        if let Some(mode) = self.options.get("mode") {
            let wanted = if self.settings.plan {
                PLAN_MODE.to_owned()
            } else {
                self.work_mode.clone().unwrap_or_default()
            };
            if !wanted.is_empty() && mode.current != wanted && mode.values.contains(&wanted) {
                changes.push(("mode", wanted, mode.mode_api));
            }
        }
        if let Some(model) = self.options.get("model")
            && !self.profile.model_option.is_empty()
            && !self.settings.model.is_empty()
        {
            let wanted = self
                .profile
                .model_option
                .replace("{model}", &self.settings.model);
            if model.current != wanted && model.values.contains(&wanted) {
                changes.push(("model", wanted, false));
            }
        }
        for (option, value, mode_api) in changes {
            if let Some(choice) = self.options.get_mut(option) {
                choice.current = value.clone();
            }
            let id = self.call(Call::Option);
            self.setting += 1;
            out.push(Out::Agent(if mode_api {
                message(
                    &id,
                    "session/set_mode",
                    json!({"sessionId": session, "modeId": value}),
                )
            } else {
                message(
                    &id,
                    "session/set_config_option",
                    json!({"sessionId": session, "configId": option, "value": value}),
                )
            }));
        }
    }

    /// Cancel the turn in progress. Permission requests still open are answered as cancelled,
    /// as the protocol requires.
    fn cancel(&mut self, out: &mut Vec<Out>) {
        let Some(turn) = self.turn.as_ref().map(|t| t.id.clone()) else {
            return;
        };
        for (_, (id, _)) in std::mem::take(&mut self.permissions) {
            out.push(Out::Agent(cancelled(&id)));
        }
        if let Some((id, _)) = self.deferred.take() {
            // Never sent: the turn ends here.
            self.calls.remove(&id);
            self.finish_turn(&turn, TurnStatus::Interrupted, out);
            return;
        }
        if let Some(session) = &self.session {
            out.push(Out::Agent(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}})));
        }
    }

    fn finish_turn(&mut self, turn: &str, status: TurnStatus, out: &mut Vec<Out>) {
        if self.turn.as_ref().is_none_or(|t| t.id != turn) {
            return;
        }
        self.end_writing(None, out);
        let mut tools: Vec<Tool> = self.tools.drain().map(|(_, tool)| tool).collect();
        tools.sort_by(|a, b| a.native_id.cmp(&b.native_id));
        for tool in tools {
            out.push(item(
                ItemPhase::Completed,
                turn,
                &tool.native_id,
                tool_item(&tool),
            ));
        }
        for (_, (id, _)) in std::mem::take(&mut self.permissions) {
            out.push(Out::Agent(cancelled(&id)));
        }
        self.turn = None;
        self.ended.clear();
        out.push(event(HarnessEvent::TurnCompleted {
            turn_id: Some(turn.into()),
            status,
        }));
    }

    fn agent_request(&mut self, method: &str, id: &Value, params: &Value, out: &mut Vec<Out>) {
        if method != "session/request_permission" {
            out.push(Out::Agent(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not supported"}})));
            return;
        }
        let options = params["options"].as_array().cloned().unwrap_or_default();
        let Some(turn) = self.turn.as_ref().map(|t| t.id.clone()) else {
            out.push(Out::Agent(cancelled(id)));
            return;
        };
        let call = &params["toolCall"];
        self.tool(call, false, out);
        let state = call["toolCallId"]
            .as_str()
            .and_then(|tool| self.tools.get(tool))
            .map_or_else(|| call.clone(), |tool| tool.state.clone());
        let kind = state["kind"].as_str().unwrap_or("other");
        let edits = matches!(kind, "edit" | "delete" | "move");
        let allowed = match self.settings.approval.as_str() {
            "full_auto" => true,
            "allow_edits" => edits,
            _ => false,
        };
        if allowed {
            out.push(Out::Agent(choose(
                id,
                &options,
                &["allow_once", "allow_always"],
            )));
            return;
        }
        let request_id = id.to_string();
        self.permissions
            .insert(request_id.clone(), (id.clone(), options));
        let detail = state["rawInput"]["command"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                let paths: Vec<&str> = state["locations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l["path"].as_str())
                    .collect();
                (!paths.is_empty()).then(|| paths.join("\n"))
            })
            .unwrap_or_default();
        let title = state["title"].as_str().unwrap_or("工具调用");
        out.push(event(HarnessEvent::ApprovalRequested {
            request_id,
            turn_id: Some(turn),
            category: if edits {
                ApprovalCategory::FileChange
            } else {
                ApprovalCategory::Command
            },
            summary: if detail.is_empty() || detail == title {
                title.to_owned()
            } else {
                format!("{title}\n{detail}")
            },
            tool: None,
            arguments: None,
        }));
    }

    fn update(&mut self, params: &Value, out: &mut Vec<Out>) {
        let update = &params["update"];
        let kind = update["sessionUpdate"].as_str().unwrap_or_default();
        if kind == "available_commands_update" {
            let commands: Vec<Value> = update["availableCommands"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| {
                    Some(json!({"name": c["name"].as_str()?, "description": c["description"].as_str().unwrap_or_default()}))
                })
                .collect();
            out.push(event(HarnessEvent::Capabilities {
                commands: Value::Array(commands),
                models: vec![],
            }));
            return;
        }
        if self.loading || params["sessionId"].as_str() != self.session.as_deref() {
            return;
        }
        match kind {
            "agent_message_chunk" | "agent_thought_chunk" => {
                if let Some(text) = update["content"]["text"].as_str() {
                    let writing = if kind == "agent_message_chunk" {
                        Writing::Reply
                    } else {
                        Writing::Thought
                    };
                    self.write(writing, update["messageId"].as_str(), text, out);
                }
            }
            "tool_call" | "tool_call_update" => self.tool(update, kind == "tool_call", out),
            "plan" => {
                let Some(turn) = &self.turn else { return };
                let text = update["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|entry| {
                        let done = if entry["status"] == "completed" {
                            "x"
                        } else {
                            " "
                        };
                        format!(
                            "- [{done}] {}",
                            entry["content"].as_str().unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let content = (
                    ItemKind::Plan,
                    ItemContent {
                        text,
                        ..Default::default()
                    },
                );
                let native_id = format!("{}:plan", turn.id);
                out.push(item(
                    ItemPhase::Completed,
                    &turn.id.clone(),
                    &native_id,
                    content,
                ));
            }
            "config_option_update" => self.read_options(update),
            "current_mode_update" => {
                if let (Some(mode), Some(current)) = (
                    self.options.get_mut("mode"),
                    update["currentModeId"].as_str(),
                ) {
                    mode.current = current.into();
                }
            }
            _ => {}
        }
    }

    /// Text the agent writes. A reply streams; reasoning arrives whole when it ends.
    fn write(
        &mut self,
        writing: Writing,
        message_id: Option<&str>,
        text: &str,
        out: &mut Vec<Out>,
    ) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        // Writing one kind ends the other, and another message ends this one.
        let continues = match writing {
            Writing::Reply => &turn.reply,
            Writing::Thought => &turn.thought,
        }
        .as_ref()
        .is_some_and(|open| open.message_id.as_deref() == message_id);
        if !continues {
            self.end_writing(None, out);
        } else {
            let other = match writing {
                Writing::Reply => Writing::Thought,
                Writing::Thought => Writing::Reply,
            };
            self.end_writing(Some(other), out);
        }
        let ended = message_id
            .and_then(|m| self.ended.get(m))
            .copied()
            .unwrap_or(0);
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let slot = match writing {
            Writing::Reply => &mut turn.reply,
            Writing::Thought => &mut turn.thought,
        };
        if slot.is_none() {
            let native_id = match message_id {
                Some(m) if ended == 0 => format!("{}:{m}", turn.id),
                Some(m) => format!("{}:{m}:{ended}", turn.id),
                None => {
                    turn.count += 1;
                    let name = if writing == Writing::Reply {
                        "reply"
                    } else {
                        "thought"
                    };
                    format!("{}:{name}:{}", turn.id, turn.count)
                }
            };
            let kind = if writing == Writing::Reply {
                ItemKind::Assistant
            } else {
                ItemKind::Reasoning
            };
            let turn_id = turn.id.clone();
            *slot = Some(Written {
                native_id: native_id.clone(),
                message_id: message_id.map(str::to_owned),
                text: String::new(),
            });
            out.push(item(
                ItemPhase::Started,
                &turn_id,
                &native_id,
                (kind, ItemContent::default()),
            ));
        }
        let Some(open) = slot.as_mut() else {
            return;
        };
        open.text.push_str(text);
        if writing == Writing::Reply {
            out.push(to_guest(Incoming::Delta {
                item_id: open.native_id.clone(),
                stream: "text",
                text: text.into(),
            }));
        }
    }

    /// End the text being written: `only` that kind, or both.
    fn end_writing(&mut self, only: Option<Writing>, out: &mut Vec<Out>) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let turn_id = turn.id.clone();
        for writing in [Writing::Thought, Writing::Reply] {
            if only.is_some_and(|o| o != writing) {
                continue;
            }
            let (slot, kind) = match writing {
                Writing::Reply => (&mut turn.reply, ItemKind::Assistant),
                Writing::Thought => (&mut turn.thought, ItemKind::Reasoning),
            };
            if let Some(written) = slot.take() {
                if let Some(m) = &written.message_id {
                    *self.ended.entry(m.clone()).or_default() += 1;
                }
                let content = ItemContent {
                    text: written.text,
                    ..Default::default()
                };
                out.push(item(
                    ItemPhase::Completed,
                    &turn_id,
                    &written.native_id,
                    (kind, content),
                ));
            }
        }
    }

    /// A tool call, or an update to one. `new` (a `tool_call`) ends the text being written.
    fn tool(&mut self, update: &Value, new: bool, out: &mut Vec<Out>) {
        let Some(id) = update["toolCallId"].as_str() else {
            return;
        };
        let Some(turn) = self.turn.as_ref().map(|t| t.id.clone()) else {
            return;
        };
        if new {
            self.end_writing(None, out);
        }
        let started = self.tools.contains_key(id);
        let tool = self.tools.entry(id.to_owned()).or_insert_with(|| Tool {
            native_id: format!("{turn}:{id}"),
            name: String::new(),
            state: json!({}),
            streamed: 0,
        });
        if let Some(fields) = update.as_object() {
            for (key, value) in fields {
                if key != "sessionUpdate" && !value.is_null() {
                    tool.state[key] = value.clone();
                }
            }
        }
        if tool.name.is_empty() {
            tool.name = update["name"]
                .as_str()
                .or_else(|| update["title"].as_str())
                .unwrap_or_default()
                .to_owned();
        }
        if !started {
            out.push(item(
                ItemPhase::Started,
                &turn,
                &tool.native_id,
                tool_item(tool),
            ));
        }
        let status = tool.state["status"].as_str().map(str::to_owned);
        match status.as_deref() {
            Some("completed" | "failed") => {
                if let Some(tool) = self.tools.remove(id) {
                    out.push(item(
                        ItemPhase::Completed,
                        &turn,
                        &tool.native_id,
                        tool_item(&tool),
                    ));
                }
            }
            _ if tool.state["kind"] == "execute" => {
                let output = text_output(&tool.state);
                if output.len() > tool.streamed && output.is_char_boundary(tool.streamed) {
                    out.push(to_guest(Incoming::Delta {
                        item_id: tool.native_id.clone(),
                        stream: "terminal_output",
                        text: output[tool.streamed..].to_owned(),
                    }));
                    tool.streamed = output.len();
                }
            }
            _ => {}
        }
    }
}

fn to_guest(incoming: Incoming) -> Out {
    Out::Guest(Box::new(incoming))
}

fn message(id: &str, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn event(event: HarnessEvent) -> Out {
    to_guest(Incoming::Event(event))
}

/// The answer to platform operation `op`, as Codex would give it.
fn reply(op: &str, result: Value) -> Out {
    to_guest(Incoming::Response {
        id: op.into(),
        response: json!({"id": op, "result": result}),
    })
}

fn failure(op: &str, error: &str) -> Out {
    to_guest(Incoming::Response {
        id: op.into(),
        response: json!({"id": op, "error": {"message": error}}),
    })
}

/// Choose the first option of the first of `kinds` offered; cancelled when none is.
fn choose(id: &Value, options: &[Value], kinds: &[&str]) -> Value {
    let option = kinds
        .iter()
        .find_map(|kind| options.iter().find(|o| o["kind"] == *kind))
        .and_then(|o| o["optionId"].as_str());
    match option {
        Some(option) => {
            json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "selected", "optionId": option}}})
        }
        None => cancelled(id),
    }
}

fn cancelled(id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "cancelled"}}})
}

fn item(
    phase: ItemPhase,
    turn: &str,
    native_id: &str,
    (kind, content): (ItemKind, ItemContent),
) -> Out {
    event(HarnessEvent::Item {
        phase,
        turn_id: Some(turn.into()),
        native_id: native_id.into(),
        kind,
        content,
        message_id: None,
        parent_native_id: None,
    })
}

/// A tool call's text output.
fn text_output(state: &Value) -> String {
    state["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["type"] == "content")
        .filter_map(|c| c["content"]["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A tool call as a platform item: file changes when it carries diffs, a command when it
/// executes one, else a tool call.
fn tool_item(tool: &Tool) -> (ItemKind, ItemContent) {
    let state = &tool.state;
    let kind = state["kind"].as_str().unwrap_or("other");
    let title = state["title"].as_str().unwrap_or(&tool.name).to_owned();
    let diffs: Vec<FileChange> = state["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["type"] == "diff")
        .filter_map(|c| {
            let path = c["path"].as_str()?;
            let new = c["newText"].as_str().unwrap_or_default();
            let old = c["oldText"].as_str();
            Some(FileChange {
                path: path.into(),
                change: match (kind, old) {
                    ("delete", _) => "delete",
                    (_, None) => "add",
                    _ => "update",
                }
                .into(),
                diff: similar::TextDiff::from_lines(old.unwrap_or_default(), new)
                    .unified_diff()
                    .header(&format!("a/{path}"), &format!("b/{path}"))
                    .to_string(),
            })
        })
        .collect();
    let mut output = text_output(state);
    if output.is_empty()
        && let Some(raw) = state["rawOutput"]["output"].as_str()
    {
        output = raw.to_owned();
    }
    if !diffs.is_empty() {
        let text = diffs
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        return (
            ItemKind::FileChange,
            ItemContent {
                text,
                file_changes: diffs,
                ..Default::default()
            },
        );
    }
    if kind == "execute" {
        let command = state["rawInput"]["command"]
            .as_str()
            .map_or(title, str::to_owned);
        return (
            ItemKind::Command,
            ItemContent {
                text: format!("{command}\n{output}"),
                name: Some(command),
                output: Some(output),
                ..Default::default()
            },
        );
    }
    (
        ItemKind::ToolCall,
        ItemContent {
            text: title,
            name: Some(tool.name.clone()),
            arguments_json: (!state["rawInput"].is_null()).then(|| state["rawInput"].to_string()),
            output: (!output.is_empty()).then_some(output),
            ..Default::default()
        },
    )
}

#[cfg(test)]
mod tests;
