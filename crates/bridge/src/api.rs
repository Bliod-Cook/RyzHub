use flutter_rust_bridge::frb;
use prost::Message;
use ryzhub_core::{
    CommandResult, Engine, Touched, ansi, diff, highlight, markdown,
    timeline::{self, View},
};
use ryzhub_proto::{command_envelope::Command, frame::Body, *};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Mutex,
};

/// How long changes wait to cross to the client together: a burst of streamed text arrives as
/// one update per display tick or two, not one per token.
const BATCH_MS: f64 = 32.0;

/// The cache record holding the engine apart from its channels; each channel has its own, so
/// a change rewrites only what changed.
const META: &str = "engine";

fn channel_key(channel: &str) -> String {
    format!("channel:{channel}")
}

pub struct ClientCore {
    inner: Mutex<State>,
}

/// A record of the client cache.
pub struct CacheRecord {
    pub key: String,
    pub value: String,
}

/// A change to the client cache: a record to store, or without a value one to remove.
pub struct CacheWrite {
    pub key: String,
    pub value: Option<String>,
}

pub struct Update {
    pub changes_json: String,
    /// Store these in one local transaction before sending any outbound frame, then call
    /// [`ClientCore::committed`].
    pub cache_writes: Vec<CacheWrite>,
    pub outbound_frames: Vec<Vec<u8>>,
    pub error: Option<String>,
    /// The commands the server finished with: `[{cid, channel, accepted, code, retryable}]`.
    pub results_json: String,
    /// The session ends soon: refresh it, then send [`ClientCore::auth_refresh`].
    pub auth_expiring: bool,
    /// The command this update queued.
    pub cid: Option<String>,
    /// Changes wait in the kernel: call [`ClientCore::flush`] after this many milliseconds.
    pub flush_in_ms: Option<u32>,
}

fn update() -> Update {
    Update {
        changes_json: "[]".into(),
        cache_writes: vec![],
        outbound_frames: vec![],
        error: None,
        results_json: "[]".into(),
        auth_expiring: false,
        cid: None,
        flush_in_ms: None,
    }
}

/// Changes that wait to cross together.
#[frb(ignore)]
#[derive(Default)]
struct Pending {
    touched: BTreeMap<String, Touched>,
    revoked: Vec<String>,
    results: Vec<CommandResult>,
    error: Option<String>,
    expiring: bool,
    /// The cache must be stored with them.
    durable: bool,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.touched.is_empty()
            && self.revoked.is_empty()
            && self.results.is_empty()
            && self.error.is_none()
            && !self.expiring
            && !self.durable
    }
}

struct State {
    engine: Engine,
    /// Channels changed since they were last stored, each with the change's number.
    dirty: BTreeMap<String, u64>,
    changes: u64,
    /// What the writes being stored now hold: channel changes and commands.
    saving: (BTreeMap<String, u64>, BTreeSet<String>),
    /// Commands whose frames are stored. Only these are sent again: a command must survive a
    /// restart before the server may run it.
    stored: BTreeSet<String>,
    /// Timelines of the conversations the client shows, as it has them.
    views: BTreeMap<String, View>,
    pending: Pending,
    /// When changes last crossed, in the client's milliseconds.
    flushed_at: f64,
}

impl State {
    fn dirty(&mut self, channel: &str) {
        self.changes += 1;
        self.dirty.insert(channel.to_owned(), self.changes);
    }

    /// The writes that bring the cache up to date: the meta record and the changed channels.
    fn writes(&mut self) -> Vec<CacheWrite> {
        let mut writes = vec![CacheWrite {
            key: META.into(),
            value: Some(self.engine.meta_json()),
        }];
        for channel in self.dirty.keys() {
            writes.push(CacheWrite {
                key: channel_key(channel),
                value: self.engine.channel_json(channel),
            });
        }
        let commands = self.engine.pending_commands.keys().cloned().collect();
        self.saving = (self.dirty.clone(), commands);
        writes
    }

    /// The last writes are stored.
    fn committed(&mut self) {
        let (channels, commands) = std::mem::take(&mut self.saving);
        for (channel, change) in channels {
            // Changed again meanwhile: the next writes carry it.
            if self.dirty.get(&channel) == Some(&change) {
                self.dirty.remove(&channel);
            }
        }
        self.stored.extend(commands);
        let pending = &self.engine.pending_commands;
        self.stored.retain(|cid| pending.contains_key(cid));
    }

    /// What changed in `channel`: its timeline when the client shows it, else its items (all of
    /// them after a snapshot, else those in `change`).
    fn channel_change(&mut self, channel: &str, change: &Touched) -> Option<Value> {
        let state = self.engine.channels.get(channel)?;
        let mut out = json!({"channel": channel,
            "actions": state.actions.values().collect::<Vec<_>>(),
            "runs": state.runs.values().collect::<Vec<_>>(),
            "read_states": state.read_states,
            "cursor": state.cursor,
            "reset": change.reset});
        if let Some(view) = self.views.get_mut(channel) {
            let touched: HashSet<String> = change.items.iter().cloned().collect();
            let diff = view.update(state, (!change.reset).then_some(&touched));
            out["timeline"] = serde_json::to_value(diff).unwrap();
        } else {
            let items: Vec<&Item> = if change.reset {
                state.items.values().collect()
            } else {
                change
                    .items
                    .iter()
                    .filter_map(|id| state.items.get(id))
                    .collect()
            };
            out["items"] = serde_json::to_value(items).unwrap();
            out["drafts"] = serde_json::to_value(&state.drafts).unwrap();
        }
        Some(out)
    }

    /// Hands over what waits: changes, results, and the cache writes that store them.
    fn flush(&mut self, now: f64, result: &mut Update) {
        let pending = std::mem::take(&mut self.pending);
        self.flushed_at = now;
        for channel in pending.touched.keys().chain(&pending.revoked) {
            self.dirty(channel);
        }
        let mut changes = vec![];
        for (channel, change) in &pending.touched {
            changes.extend(self.channel_change(channel, change));
        }
        for channel in &pending.revoked {
            self.views.remove(channel);
            changes.push(json!({"channel":channel,"revoked":true}));
        }
        result.changes_json = serde_json::to_string(&changes).unwrap();
        if pending.durable {
            result.cache_writes = self.writes();
        }
        result.error = pending.error;
        result.results_json = serde_json::to_string(&pending.results).unwrap();
        result.auth_expiring = pending.expiring;
    }

    /// Whether `frame` may go out: any but a command not stored yet, or a frame for a
    /// conversation still being created.
    fn sendable(&self, frame: &Frame) -> bool {
        !self.engine.held(frame)
            && match &frame.body {
                Some(Body::Command(command)) => self.stored.contains(&command.cid),
                _ => true,
            }
    }
}

impl ClientCore {
    #[frb(sync)]
    pub fn new(records: Vec<CacheRecord>) -> Result<Self, String> {
        let (mut meta, mut channels) = (None, vec![]);
        for record in records {
            if record.key == META {
                meta = Some(record.value);
            } else if let Some(name) = record.key.strip_prefix("channel:") {
                channels.push((name.to_owned(), record.value));
            }
        }
        let engine = Engine::from_records(meta.as_deref(), channels).map_err(|e| e.to_string())?;
        Ok(Self {
            inner: Mutex::new(State {
                stored: engine.pending_commands.keys().cloned().collect(),
                dirty: BTreeMap::new(),
                changes: 0,
                saving: Default::default(),
                engine,
                views: BTreeMap::new(),
                pending: Pending::default(),
                flushed_at: f64::NEG_INFINITY,
            }),
        })
    }
    #[frb(sync)]
    pub fn hello(&self, device_id: String) -> Vec<u8> {
        frame(
            "",
            Body::Hello(Hello {
                protocol_major: PROTOCOL_MAJOR,
                device_id,
                ..Default::default()
            }),
        )
        .encode_to_vec()
    }
    /// Continue the connection under a refreshed access token.
    #[frb(sync)]
    pub fn auth_refresh(&self, token: String) -> Vec<u8> {
        frame("", Body::AuthRefresh(AuthRefresh { token })).encode_to_vec()
    }
    /// The last update's cache writes are stored.
    #[frb(sync)]
    pub fn committed(&self) {
        self.inner.lock().unwrap().committed();
    }
    #[frb(sync)]
    pub fn merge_history(&self, channel: String, items_json: String) -> Result<Update, String> {
        let items = serde_json::from_str(&items_json).map_err(|e| e.to_string())?;
        let mut state = self.inner.lock().unwrap();
        let changed = state.engine.merge_history(&channel, items);
        let change = Touched {
            items: changed.into_iter().map(|i| i.id).collect(),
            reset: false,
        };
        let mut result = update();
        let Some(changes) = state.channel_change(&channel, &change) else {
            return Ok(result);
        };
        result.changes_json = json!([changes]).to_string();
        if !change.items.is_empty() {
            state.dirty(&channel);
            result.cache_writes = state.writes();
        }
        Ok(result)
    }
    #[frb(sync)]
    pub fn subscribe(&self, channel: String) -> Update {
        let mut state = self.inner.lock().unwrap();
        let mut outbound = Vec::new();
        if channel.starts_with("conv:") {
            let old: Vec<_> = state
                .engine
                .channels
                .iter()
                .filter(|(c, s)| c.starts_with("conv:") && **c != channel && s.subscribed)
                .map(|(c, _)| c.clone())
                .collect();
            for c in old {
                outbound.push(state.engine.unsubscribe(c.clone()).encode_to_vec());
                state.dirty(&c);
            }
            // The client shows this conversation's timeline from what the cache holds.
            state.views.clear();
            state.views.insert(channel.clone(), View::default());
        }
        let subscribe = state.engine.subscribe(channel.clone());
        // A conversation still being created is subscribed once it exists.
        if !state.engine.held(&subscribe) {
            outbound.push(subscribe.encode_to_vec());
        }
        state.dirty(&channel);
        let mut result = update();
        if let Some(changes) = state.channel_change(
            &channel,
            &Touched {
                items: BTreeSet::new(),
                reset: true,
            },
        ) {
            result.changes_json = json!([changes]).to_string();
        }
        result.cache_writes = state.writes();
        result.outbound_frames = outbound;
        result
    }
    #[frb(sync)]
    /// `config_json` carries settings not yet confirmed by the server, so they
    /// apply to this message's run regardless of command ordering. `attachment_ids`
    /// name files of the sender's library; the text may then be empty.
    pub fn send_text(
        &self,
        channel: String,
        text: String,
        config_json: Option<String>,
        attachment_ids: Vec<String>,
    ) -> Update {
        self.command(
            channel,
            Command::SendMessage(SendMessage {
                text,
                config_json: config_json.unwrap_or_default(),
                attachment_ids,
            }),
        )
    }
    #[frb(sync)]
    pub fn decide(
        &self,
        channel: String,
        action_id: String,
        run_epoch: u64,
        decision: String,
    ) -> Update {
        self.command(
            channel,
            Command::DecideApproval(DecideApproval {
                action_id,
                run_epoch,
                decision,
            }),
        )
    }
    #[frb(sync)]
    pub fn cancel(&self, channel: String) -> Update {
        self.command(channel, Command::Cancel(Cancel {}))
    }
    #[frb(sync)]
    pub fn steer(&self, channel: String, text: String, attachment_ids: Vec<String>) -> Update {
        self.command(
            channel,
            Command::Steer(Steer {
                text,
                attachment_ids,
            }),
        )
    }
    #[frb(sync)]
    pub fn answer_question(
        &self,
        channel: String,
        action_id: String,
        run_epoch: u64,
        answer: String,
    ) -> Update {
        self.command(
            channel,
            Command::AnswerQuestion(AnswerQuestion {
                action_id,
                run_epoch,
                answer,
            }),
        )
    }
    #[frb(sync)]
    pub fn configure(&self, channel: String, config_json: String) -> Update {
        self.command(
            channel,
            Command::UpdateRunConfig(UpdateRunConfig { config_json }),
        )
    }
    #[frb(sync)]
    pub fn compact(&self, channel: String) -> Update {
        self.command(channel, Command::Compact(Compact {}))
    }
    #[frb(sync)]
    pub fn fork(&self, channel: String, item_id: String) -> Update {
        self.command(channel, Command::Fork(Fork { item_id }))
    }
    /// Creates conversation `conversation_id` (see [`new_conversation_id`]) with its first
    /// message, on the user's channel `channel`. Queued like any command, it may be created
    /// offline; frames for the conversation wait until the server has answered it.
    #[frb(sync)]
    #[allow(clippy::too_many_arguments)]
    pub fn create_run(
        &self,
        channel: String,
        conversation_id: String,
        mode: String,
        run_config_json: String,
        project_id: Option<String>,
        temporary: bool,
        title: String,
        text: String,
        attachment_ids: Vec<String>,
    ) -> Update {
        self.command(
            channel,
            Command::CreateRun(CreateRun {
                conversation_id,
                mode,
                run_config_json,
                project_id: project_id.unwrap_or_default(),
                temporary,
                title,
                first_message: Some(SendMessage {
                    text,
                    config_json: String::new(),
                    attachment_ids,
                }),
            }),
        )
    }
    /// Undoes the memory change of `event_id`, on the user's channel `channel`.
    #[frb(sync)]
    pub fn undo_memory_update(&self, channel: String, event_id: String) -> Update {
        self.command(
            channel,
            Command::UndoMemoryUpdate(UndoMemoryUpdate { event_id }),
        )
    }
    /// Runs a plugin's command in conversation `channel`, with its parameters as JSON.
    #[frb(sync)]
    pub fn invoke_menu_command(
        &self,
        channel: String,
        installation_id: String,
        command: String,
        params_json: String,
        attachment_ids: Vec<String>,
    ) -> Update {
        self.command(
            channel,
            Command::InvokeMenuCommand(InvokeMenuCommand {
                installation_id,
                command,
                params_json,
                attachment_ids,
            }),
        )
    }
    /// The user has read conversation `channel` up to `read_seq`.
    #[frb(sync)]
    pub fn mark_read(&self, channel: String, read_seq: u64) -> Update {
        self.command(channel, Command::MarkRead(MarkRead { read_seq }))
    }
    fn command(&self, channel: String, command: Command) -> Update {
        let mut state = self.inner.lock().unwrap();
        let cid = uuid::Uuid::now_v7().to_string();
        let f = state.engine.enqueue(
            channel,
            CommandEnvelope {
                cid: cid.clone(),
                command: Some(command),
            },
        );
        let mut result = update();
        result.cache_writes = state.writes();
        if !state.engine.held(&f) {
            result.outbound_frames = vec![f.encode_to_vec()];
        }
        result.cid = Some(cid);
        result
    }
    /// Stored commands not acknowledged yet, to send again.
    #[frb(sync)]
    pub fn retry_pending(&self) -> Vec<Vec<u8>> {
        let state = self.inner.lock().unwrap();
        state
            .engine
            .pending_commands
            .values()
            .filter(|f| state.sendable(f))
            .map(|f| f.encode_to_vec())
            .collect()
    }
    /// Applies frames as they arrive (`now_ms` is the client's clock). What they change waits
    /// in the kernel to cross with what follows it within [`BATCH_MS`]: the update says when to
    /// [`ClientCore::flush`]. Frames to send go out at once.
    #[frb(sync)]
    pub fn ingest(&self, frames: Vec<Vec<u8>>, now_ms: f64) -> Result<Update, String> {
        let mut state = self.inner.lock().unwrap();
        let mut result = update();
        for bytes in frames {
            let bytes = ryzhub_core::frame_bytes(&bytes)?;
            let f = Frame::decode(&*bytes).map_err(|e| e.to_string())?;
            state.pending.durable |= matches!(
                f.body,
                Some(
                    Body::Welcome(_)
                        | Body::Event(_)
                        | Body::Snapshot(_)
                        | Body::CommandAck(_)
                        | Body::CommandRejected(_)
                        | Body::SubscriptionRevoked(_)
                )
            );
            if matches!(f.body, Some(Body::Welcome(_))) {
                let frames = state.engine.reconnect();
                let sendable: Vec<Vec<u8>> = frames
                    .iter()
                    .filter(|f| state.sendable(f))
                    .map(|f| f.encode_to_vec())
                    .collect();
                result.outbound_frames.extend(sendable);
            }
            let effects = state.engine.apply(f);
            let outbound: Vec<Vec<u8>> = effects
                .outbound
                .iter()
                .filter(|f| state.sendable(f))
                .map(|f| f.encode_to_vec())
                .collect();
            result.outbound_frames.extend(outbound);
            let pending = &mut state.pending;
            for (channel, change) in effects.touched {
                let entry = pending.touched.entry(channel).or_default();
                entry.items.extend(change.items);
                entry.reset |= change.reset;
            }
            pending.revoked.extend(effects.revoked);
            pending.results.extend(effects.results);
            pending.expiring |= effects.auth_expiring;
            if effects.error.is_some() {
                pending.error = effects.error;
            }
        }
        if !state.pending.is_empty() {
            let waited = now_ms - state.flushed_at;
            if waited >= BATCH_MS {
                state.flush(now_ms, &mut result);
            } else {
                result.flush_in_ms = Some((BATCH_MS - waited).ceil().max(1.0) as u32);
            }
        }
        Ok(result)
    }

    /// The changes waiting in the kernel; none when nothing waits.
    #[frb(sync)]
    pub fn flush(&self, now_ms: f64) -> Update {
        let mut state = self.inner.lock().unwrap();
        let mut result = update();
        if !state.pending.is_empty() {
            state.flush(now_ms, &mut result);
        }
        result
    }
}

/// `text` rendered as messages show it: `{blocks, chips}`. `cited` are the pages a reply cites;
/// `streaming` closes what its end leaves open.
#[frb(sync)]
pub fn render_markdown(text: String, cited: Vec<String>, streaming: bool) -> String {
    serde_json::to_string(&markdown::render(&text, &cited, streaming)).unwrap()
}

/// Rows and items for `items_json`, a stretch of a conversation shown apart from its live
/// timeline: `{rows, items}` as a timeline hands them over.
#[frb(sync)]
pub fn timeline_snapshot(items_json: String) -> Result<String, String> {
    let items: Vec<Item> = serde_json::from_str(&items_json).map_err(|e| e.to_string())?;
    let (rows, items) = timeline::snapshot(items);
    Ok(json!({"rows": rows, "items": items}).to_string())
}

static HIGHLIGHTER: Mutex<Option<highlight::Highlighter>> = Mutex::new(None);

/// `code` coloured as `language`: `[[text, class], …]` with highlight.js class names. `key`
/// names the code block across calls, so a growing block is parsed only from its last complete
/// line. None when the language is unknown, the grammars are not loaded or the code is long.
#[frb(sync)]
pub fn highlight(key: String, language: String, code: String) -> Option<String> {
    let mut highlighter = HIGHLIGHTER.lock().unwrap();
    let spans = highlighter
        .get_or_insert_with(Default::default)
        .highlight(&key, &language, &code)?;
    Some(serde_json::to_string(&spans).unwrap())
}

/// Whether code can be coloured; on the web, not until [`load_syntaxes`].
#[frb(sync)]
pub fn syntaxes_ready() -> bool {
    highlight::ready()
}

/// Loads the grammars the web fetches on first use.
#[frb(sync)]
pub fn load_syntaxes(dump: Vec<u8>) -> Result<(), String> {
    highlight::load_syntaxes(&dump)
}

/// Terminal output as spans: `[{text, fg?, bg?, style?}]`.
#[frb(sync)]
pub fn ansi_spans(text: String) -> String {
    serde_json::to_string(&ansi::parse(&text)).unwrap()
}

/// A unified diff as lines: `[{kind, text, old?, new?}]`.
#[frb(sync)]
pub fn diff_lines(text: String) -> String {
    serde_json::to_string(&diff::parse(&text)).unwrap()
}

/// An id for a conversation this client creates.
#[frb(sync)]
pub fn new_conversation_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

#[frb(sync)]
pub fn reconnect_delay(attempt: u32, random: u32) -> u32 {
    ryzhub_core::reconnect_delay_ms(attempt, random as u64) as u32
}
