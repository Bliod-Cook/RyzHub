use flutter_rust_bridge::frb;
use prost::Message;
use ryzhub_core::{Engine, Touched};
use ryzhub_proto::{command_envelope::Command, frame::Body, *};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

/// The cache record holding the engine apart from its channels; each channel has its own, so
/// a change rewrites only what changed.
const META: &str = "engine";

fn channel_key(channel: &str) -> String {
    format!("channel:{channel}")
}

pub struct ClientCore {
    inner: Mutex<State>,
}

/// A record of the client cache. The empty key is a whole engine cached before channels had
/// records of their own.
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
    }
}

struct State {
    engine: Engine,
    /// Channels changed since they were last stored, each with the change's number.
    dirty: BTreeMap<String, u64>,
    changes: u64,
    /// What the writes being stored now hold: channel changes, commands, and whether they
    /// replace a cache from before records.
    saving: (BTreeMap<String, u64>, BTreeSet<String>, bool),
    /// Commands whose frames are stored. Only these are sent again: a command must survive a
    /// restart before the server may run it.
    stored: BTreeSet<String>,
    /// The cache still holds a whole engine from before records.
    legacy: bool,
}

impl State {
    fn dirty(&mut self, channel: &str) {
        self.changes += 1;
        self.dirty.insert(channel.to_owned(), self.changes);
    }

    /// The writes that bring the cache up to date: the meta record, the changed channels and
    /// the removal of a record from before.
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
        if self.legacy {
            writes.push(CacheWrite {
                key: String::new(),
                value: None,
            });
        }
        let commands = self.engine.pending_commands.keys().cloned().collect();
        self.saving = (self.dirty.clone(), commands, self.legacy);
        writes
    }

    /// The last writes are stored.
    fn committed(&mut self) {
        let (channels, commands, legacy) = std::mem::take(&mut self.saving);
        for (channel, change) in channels {
            // Changed again meanwhile: the next writes carry it.
            if self.dirty.get(&channel) == Some(&change) {
                self.dirty.remove(&channel);
            }
        }
        self.stored.extend(commands);
        let pending = &self.engine.pending_commands;
        self.stored.retain(|cid| pending.contains_key(cid));
        self.legacy &= !legacy;
    }

    /// Whether `frame` may go out: any but a command not stored yet.
    fn sendable(&self, frame: &Frame) -> bool {
        match &frame.body {
            Some(Body::Command(command)) => self.stored.contains(&command.cid),
            _ => true,
        }
    }
}

impl ClientCore {
    #[frb(sync)]
    pub fn new(records: Vec<CacheRecord>) -> Result<Self, String> {
        let (mut meta, mut legacy, mut channels) = (None, None, vec![]);
        for record in records {
            if record.key == META {
                meta = Some(record.value);
            } else if record.key.is_empty() {
                legacy = Some(record.value);
            } else if let Some(name) = record.key.strip_prefix("channel:") {
                channels.push((name.to_owned(), record.value));
            }
        }
        let engine = match (&meta, &legacy) {
            (None, Some(legacy)) => Engine::restore(legacy),
            _ => Engine::from_records(meta.as_deref(), channels),
        }
        .map_err(|e| e.to_string())?;
        let mut state = State {
            stored: engine.pending_commands.keys().cloned().collect(),
            legacy: legacy.is_some(),
            dirty: BTreeMap::new(),
            changes: 0,
            saving: Default::default(),
            engine,
        };
        // A cache from before records becomes records with the first save.
        if meta.is_none() && state.legacy {
            let names: Vec<String> = state.engine.channels.keys().cloned().collect();
            for name in names {
                state.dirty(&name);
            }
        }
        Ok(Self {
            inner: Mutex::new(state),
        })
    }
    #[frb(sync)]
    pub fn hello(&self, device_id: String) -> Vec<u8> {
        frame(
            "",
            Body::Hello(Hello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                capabilities: vec!["text/0".into()],
                resume_token: self.inner.lock().unwrap().engine.resume_token.clone(),
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
    pub fn view_json(&self) -> String {
        serde_json::to_string(&self.inner.lock().unwrap().engine).unwrap()
    }
    #[frb(sync)]
    pub fn merge_history(&self, channel: String, items_json: String) -> Result<Update, String> {
        let items = serde_json::from_str(&items_json).map_err(|e| e.to_string())?;
        let mut state = self.inner.lock().unwrap();
        let changed = state.engine.merge_history(&channel, items);
        let Some(channel_state) = state.engine.channels.get(&channel) else {
            return Ok(update());
        };
        let changes = json!([{"channel":channel,"items":changed,
            "actions":channel_state.actions.values().collect::<Vec<_>>(),
            "runs":channel_state.runs.values().collect::<Vec<_>>(),"cursor":channel_state.cursor}]);
        let mut result = update();
        result.changes_json = changes.to_string();
        if !changed.is_empty() {
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
        }
        outbound.push(state.engine.subscribe(channel.clone()).encode_to_vec());
        state.dirty(&channel);
        let mut result = update();
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
    pub fn side_query(&self, channel: String, text: String) -> Update {
        self.command(channel, Command::SideQuery(SideQuery { text }))
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
        result.outbound_frames = vec![f.encode_to_vec()];
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
    /// Flutter batches incoming frames for one display tick, then receives what changed: only
    /// the channels and items these frames touched.
    #[frb(sync)]
    pub fn ingest(&self, frames: Vec<Vec<u8>>) -> Result<Update, String> {
        let mut state = self.inner.lock().unwrap();
        let mut outbound_frames = vec![];
        let mut durable = false;
        let mut touched: BTreeMap<String, Touched> = BTreeMap::new();
        let mut revoked = vec![];
        let mut results = vec![];
        let mut expiring = false;
        for bytes in frames {
            let f = Frame::decode(bytes.as_slice()).map_err(|e| e.to_string())?;
            durable |= matches!(
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
                outbound_frames.extend(
                    frames
                        .iter()
                        .filter(|f| state.sendable(f))
                        .map(|f| f.encode_to_vec()),
                );
            }
            let effects = state.engine.apply(f);
            outbound_frames.extend(effects.outbound.into_iter().map(|f| f.encode_to_vec()));
            for (channel, change) in effects.touched {
                let entry = touched.entry(channel).or_default();
                entry.items.extend(change.items);
                entry.reset |= change.reset;
            }
            revoked.extend(effects.revoked);
            results.extend(effects.results);
            expiring |= effects.auth_expiring.is_some();
        }
        for channel in touched.keys().chain(&revoked) {
            state.dirty(channel);
        }
        let mut changes = vec![];
        for (channel, change) in &touched {
            let Some(channel_state) = state.engine.channels.get(channel) else {
                continue;
            };
            let items: Vec<&Item> = if change.reset {
                channel_state.items.values().collect()
            } else {
                change
                    .items
                    .iter()
                    .filter_map(|id| channel_state.items.get(id))
                    .collect()
            };
            changes.push(json!({"channel":channel,"reset":change.reset,"items":items,
                "actions":channel_state.actions.values().collect::<Vec<_>>(),
                "runs":channel_state.runs.values().collect::<Vec<_>>(),"cursor":channel_state.cursor}));
        }
        for channel in &revoked {
            changes.push(json!({"channel":channel,"revoked":true}));
        }
        let mut result = update();
        result.changes_json = serde_json::to_string(&changes).unwrap();
        if durable {
            result.cache_writes = state.writes();
        }
        result.outbound_frames = outbound_frames;
        result.error = state.engine.last_error.take();
        result.results_json = serde_json::to_string(&results).unwrap();
        result.auth_expiring = expiring;
        Ok(result)
    }
}
#[frb(sync)]
pub fn reconnect_delay(attempt: u32, random: u32) -> u32 {
    ryzhub_core::reconnect_delay_ms(attempt, random as u64) as u32
}
