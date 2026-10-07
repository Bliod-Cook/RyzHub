//! Deterministic client projection. No clocks, sockets, filesystem, or platform APIs.
use ryzhub_proto::{event_envelope::Event, frame::Body, *};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const MAX_BUFFERED: usize = 256;
const MAX_BUFFERED_BYTES: usize = 1024 * 1024;

/// Bumped when cached projections may lack fields clients now read. 1: item `detail`; 2: the
/// files user messages carry; 3: the files a reply links to; 4: whether a conversation is pinned.
pub const CACHE_SCHEMA: u32 = 4;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Channel {
    pub subscribed: bool,
    pub cursor: u64,
    pub initialized: bool,
    pub items: BTreeMap<String, Item>,
    pub actions: BTreeMap<String, PendingAction>,
    pub runs: BTreeMap<String, RunState>,
    pub read_states: BTreeMap<String, u64>,
    #[serde(skip)]
    waiting_events: BTreeMap<u64, EventEnvelope>,
    #[serde(skip)]
    waiting_streams: Vec<Body>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Engine {
    #[serde(default)]
    pub resume_token: String,
    pub channels: BTreeMap<String, Channel>,
    /// Persist this alongside projections and cursors, atomically, before sending.
    pub pending_commands: BTreeMap<String, Frame>,
    pub last_error: Option<String>,
    /// The `CACHE_SCHEMA` this state was cached under; absent in older caches.
    pub schema: u32,
}

#[derive(Debug, Default)]
pub struct Effects {
    pub outbound: Vec<Frame>,
    pub changed: bool,
    /// Channels whose projection changed, with what changed in each.
    pub touched: BTreeMap<String, Touched>,
    /// Channels taken away from this client.
    pub revoked: Vec<String>,
    /// Commands the server has finished with.
    pub results: Vec<CommandResult>,
    /// When the session ends unless refreshed, in milliseconds since the epoch.
    pub auth_expiring: Option<i64>,
}

/// What changed in a channel.
#[derive(Debug, Default)]
pub struct Touched {
    /// Items that changed.
    pub items: BTreeSet<String>,
    /// A snapshot replaced the whole projection.
    pub reset: bool,
}

impl Effects {
    fn touch(&mut self, channel: &str) -> &mut Touched {
        self.touched.entry(channel.to_owned()).or_default()
    }
}

/// A command the server accepted or refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommandResult {
    pub cid: String,
    pub channel: String,
    pub accepted: bool,
    /// Why it was refused.
    pub code: String,
    /// A refusal the same command may retry.
    pub retryable: bool,
}

/// The engine apart from its channels: channels persist one record each, so a change to one
/// rewrites only it.
#[derive(Serialize)]
struct MetaRef<'a> {
    resume_token: &'a str,
    pending_commands: &'a BTreeMap<String, Frame>,
    schema: u32,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Meta {
    resume_token: String,
    pending_commands: BTreeMap<String, Frame>,
    schema: u32,
}

impl Engine {
    /// The engine from its cache. Channels cached under an older schema keep showing their
    /// items but resubscribe without a cursor, so a fresh snapshot replaces them; pending
    /// commands survive.
    pub fn restore(cache_json: &str) -> serde_json::Result<Self> {
        let engine: Engine = if cache_json.is_empty() {
            Engine::default()
        } else {
            serde_json::from_str(cache_json)?
        };
        Ok(engine.upgraded())
    }

    /// The engine from its records: [`Engine::meta_json`] and each channel's
    /// [`Engine::channel_json`]. No meta is a fresh engine.
    pub fn from_records(
        meta: Option<&str>,
        channels: impl IntoIterator<Item = (String, String)>,
    ) -> serde_json::Result<Self> {
        let Some(meta) = meta else {
            return Ok(Engine::default().upgraded());
        };
        let meta: Meta = serde_json::from_str(meta)?;
        let mut engine = Engine {
            resume_token: meta.resume_token,
            pending_commands: meta.pending_commands,
            schema: meta.schema,
            ..Default::default()
        };
        for (name, json) in channels {
            engine.channels.insert(name, serde_json::from_str(&json)?);
        }
        Ok(engine.upgraded())
    }

    /// What persists beside the channels: the resume token and the commands not yet
    /// acknowledged.
    pub fn meta_json(&self) -> String {
        serde_json::to_string(&MetaRef {
            resume_token: &self.resume_token,
            pending_commands: &self.pending_commands,
            schema: self.schema,
        })
        .unwrap()
    }

    /// A channel's record, or None once the channel is gone.
    pub fn channel_json(&self, channel: &str) -> Option<String> {
        self.channels
            .get(channel)
            .map(|state| serde_json::to_string(state).unwrap())
    }

    /// Channels cached under an older schema keep their items but resubscribe without a
    /// cursor, so a fresh snapshot replaces them.
    fn upgraded(mut self) -> Self {
        if self.schema < CACHE_SCHEMA {
            for state in self.channels.values_mut() {
                state.initialized = false;
            }
            self.schema = CACHE_SCHEMA;
        }
        self
    }

    /// History fills older projections without advancing the live replay cursor.
    pub fn merge_history(&mut self, channel: &str, items: Vec<Item>) -> Vec<Item> {
        let Some(state) = self.channels.get_mut(channel) else {
            return vec![];
        };
        let mut changed = vec![];
        for item in items {
            if state
                .items
                .get(&item.id)
                .is_none_or(|old| item.revision > old.revision)
            {
                state.items.insert(item.id.clone(), item.clone());
                changed.push(item);
            }
        }
        changed
    }

    pub fn subscribe(&mut self, channel: String) -> Frame {
        let state = self.channels.entry(channel.clone()).or_default();
        state.subscribed = true;
        frame(
            channel,
            Body::Subscribe(Subscribe {
                since_seq: state.initialized.then_some(state.cursor),
            }),
        )
    }

    pub fn unsubscribe(&mut self, channel: String) -> Frame {
        if let Some(state) = self.channels.get_mut(&channel) {
            state.subscribed = false;
        }
        frame(channel, Body::Unsubscribe(Unsubscribe {}))
    }

    pub fn reconnect(&mut self) -> Vec<Frame> {
        // Drop ephemeral buffers: replay + ItemFold rebuild them.
        for state in self.channels.values_mut() {
            state.waiting_events.clear();
            state.waiting_streams.clear();
        }
        let channels: Vec<_> = self
            .channels
            .iter()
            .filter(|(_, s)| s.subscribed)
            .map(|(c, _)| c.clone())
            .collect();
        let mut frames: Vec<_> = self
            .channels
            .iter()
            .filter(|(_, s)| !s.subscribed)
            .map(|(c, _)| frame(c, Body::Unsubscribe(Unsubscribe {})))
            .collect();
        frames.extend(channels.into_iter().map(|c| self.subscribe(c)));
        frames.extend(self.pending_commands.values().cloned());
        frames
    }

    pub fn enqueue(&mut self, channel: String, command: CommandEnvelope) -> Frame {
        if let Some(existing) = self.pending_commands.get(&command.cid) {
            return existing.clone();
        }
        let cid = command.cid.clone();
        let command = frame(channel, Body::Command(command));
        self.pending_commands.insert(cid, command.clone());
        command
    }

    pub fn apply(&mut self, input: Frame) -> Effects {
        let mut effects = Effects::default();
        let Some(body) = input.body else {
            return effects;
        };
        match body {
            Body::Welcome(w) => {
                self.resume_token = w.resume_token;
                effects.changed = true;
            }
            Body::Ping(p) => effects
                .outbound
                .push(frame("", Body::Pong(Pong { nonce: p.nonce }))),
            Body::CommandAck(a) => {
                let channel = self
                    .pending_commands
                    .remove(&a.cid)
                    .map_or(input.channel, |f| f.channel);
                effects.results.push(CommandResult {
                    cid: a.cid,
                    channel,
                    accepted: true,
                    code: String::new(),
                    retryable: false,
                });
                effects.changed = true;
            }
            Body::CommandRejected(r) => {
                let channel = self
                    .pending_commands
                    .get(&r.cid)
                    .map_or(input.channel, |f| f.channel.clone());
                if !r.retryable {
                    self.pending_commands.remove(&r.cid);
                }
                effects.results.push(CommandResult {
                    cid: r.cid,
                    channel,
                    accepted: false,
                    code: r.code,
                    retryable: r.retryable,
                });
                effects.changed = true;
            }
            Body::AuthExpiring(a) => effects.auth_expiring = Some(a.expires_at_ms),
            Body::ProtocolError(e) => {
                self.last_error = Some(e.code);
                effects.changed = true;
            }
            Body::SubscriptionRevoked(_) => {
                self.channels.remove(&input.channel);
                self.pending_commands
                    .retain(|_, f| f.channel != input.channel);
                effects.revoked.push(input.channel);
                effects.changed = true;
            }
            Body::ChannelLagged(_) => {
                if self
                    .channels
                    .get(&input.channel)
                    .is_some_and(|s| s.subscribed)
                {
                    effects.outbound.push(self.subscribe(input.channel));
                }
            }
            body => {
                let Some(state) = self.channels.get_mut(&input.channel) else {
                    return effects;
                };
                let mut resync = false;
                let mut touched = Touched::default();
                let mut changed = false;
                match body {
                    Body::Snapshot(s) => {
                        // A delayed snapshot cannot roll a live channel backward.
                        if !state.initialized || s.head_seq >= state.cursor {
                            state.cursor = s.head_seq;
                            state.initialized = true;
                            state.items = s.items.into_iter().map(|i| (i.id.clone(), i)).collect();
                            state.actions = s
                                .pending_actions
                                .into_iter()
                                .map(|a| (a.id.clone(), a))
                                .collect();
                            state.runs =
                                s.runs.into_iter().map(|r| (r.run_id.clone(), r)).collect();
                            state.read_states = s
                                .read_states
                                .into_iter()
                                .map(|r| (r.conversation_id, r.read_seq))
                                .collect();
                            state.waiting_events.retain(|seq, _| *seq > s.head_seq);
                            state.waiting_streams.clear();
                            drain_events(state, &mut touched.items);
                            touched.reset = true;
                            changed = true;
                        }
                    }
                    Body::Event(event) => {
                        if event.seq > state.cursor {
                            if event.seq == state.cursor.saturating_add(1) {
                                touched.items.extend(apply_event(state, event));
                                drain_events(state, &mut touched.items);
                                changed = true;
                            } else {
                                if state.waiting_events.len() < MAX_BUFFERED {
                                    state.waiting_events.insert(event.seq, event);
                                }
                                resync = true;
                            }
                        }
                    }
                    body @ (Body::Delta(_) | Body::ItemFold(_)) => {
                        match apply_stream(state, &body) {
                            StreamResult::Applied => {
                                touched.items.extend(stream_item(&body));
                                changed = true;
                            }
                            StreamResult::Future => {
                                if state.waiting_streams.len() < MAX_BUFFERED
                                    && buffered_bytes(state) + body_bytes(&body)
                                        <= MAX_BUFFERED_BYTES
                                {
                                    state.waiting_streams.push(body);
                                } else {
                                    state.waiting_streams.clear();
                                    resync = true;
                                }
                            }
                            StreamResult::Gap => resync = true,
                            StreamResult::Ignored => {}
                        }
                    }
                    Body::SubscriptionSynced(s) if state.cursor < s.head_seq => {
                        resync = true;
                    }
                    _ => {}
                }
                let pending = std::mem::take(&mut state.waiting_streams);
                for body in pending {
                    match apply_stream(state, &body) {
                        StreamResult::Future => state.waiting_streams.push(body),
                        StreamResult::Applied => {
                            touched.items.extend(stream_item(&body));
                            changed = true;
                        }
                        StreamResult::Gap => resync = true,
                        StreamResult::Ignored => {}
                    }
                }
                if changed {
                    let entry = effects.touch(&input.channel);
                    entry.items.extend(touched.items);
                    entry.reset |= touched.reset;
                    effects.changed = true;
                }
                if resync {
                    effects.outbound.push(self.subscribe(input.channel));
                }
            }
        }
        effects
    }
}

/// Apply `event`, returning the item it changed, if any.
fn apply_event(state: &mut Channel, event: EventEnvelope) -> Option<String> {
    state.cursor = event.seq;
    state.initialized = true;
    match event.event {
        Some(Event::ItemUpserted(item)) => {
            if state
                .items
                .get(&item.id)
                .is_none_or(|old| item.revision > old.revision)
            {
                let id = item.id.clone();
                state.items.insert(id.clone(), item);
                return Some(id);
            }
        }
        Some(Event::PendingActionChanged(action)) => {
            state.actions.insert(action.id.clone(), action);
        }
        Some(Event::RunStateChanged(run)) => {
            state.runs.insert(run.run_id.clone(), run);
        }
        Some(Event::ReadStateChanged(read)) => {
            let seq = state.read_states.entry(read.conversation_id).or_default();
            *seq = (*seq).max(read.read_seq);
        }
        None => {}
    }
    None
}

/// Apply the buffered events that now follow the cursor, noting the items they change.
fn drain_events(state: &mut Channel, items: &mut BTreeSet<String>) {
    while let Some(event) = state
        .waiting_events
        .remove(&(state.cursor.saturating_add(1)))
    {
        items.extend(apply_event(state, event));
    }
}

/// The item a delta or fold streams into.
fn stream_item(body: &Body) -> Option<String> {
    match body {
        Body::Delta(d) => Some(d.item_id.clone()),
        Body::ItemFold(f) => Some(f.item_id.clone()),
        _ => None,
    }
}

enum StreamResult {
    Applied,
    Future,
    Gap,
    Ignored,
}

fn apply_stream(state: &mut Channel, body: &Body) -> StreamResult {
    let (id, stream, part, base, revision, text, offset) = match body {
        Body::Delta(d) => (
            &d.item_id,
            &d.stream,
            d.part_index,
            d.base_seq,
            d.revision,
            &d.text,
            Some(d.offset),
        ),
        Body::ItemFold(f) => (
            &f.item_id,
            &f.stream,
            f.part_index,
            f.base_seq,
            f.revision,
            &f.text,
            None,
        ),
        _ => return StreamResult::Ignored,
    };
    // M0 negotiates text/part 0 only. Other streams do not corrupt the text projection.
    if stream != "text" || part != 0 {
        return StreamResult::Ignored;
    }
    if base > state.cursor {
        return StreamResult::Future;
    }
    let Some(item) = state.items.get_mut(id) else {
        return StreamResult::Gap;
    };
    if item.state != "streaming" || revision < item.revision || base < item.last_seq {
        return StreamResult::Ignored;
    }
    if revision > item.revision || base > item.last_seq {
        return StreamResult::Gap;
    }
    let Some(offset) = offset else {
        if text.len() <= item.text.len() {
            return StreamResult::Ignored;
        }
        if !text.starts_with(&item.text) {
            return StreamResult::Gap;
        }
        item.text = text.clone();
        return StreamResult::Applied;
    };
    let Ok(offset) = usize::try_from(offset) else {
        return StreamResult::Gap;
    };
    if offset > item.text.len() || !item.text.is_char_boundary(offset) {
        return StreamResult::Gap;
    }
    let overlap = (item.text.len() - offset).min(text.len());
    if item.text.as_bytes()[offset..offset + overlap] != text.as_bytes()[..overlap]
        || !text.is_char_boundary(overlap)
    {
        return StreamResult::Gap;
    }
    item.text.push_str(&text[overlap..]);
    StreamResult::Applied
}

fn body_bytes(body: &Body) -> usize {
    match body {
        Body::Delta(d) => d.text.len(),
        Body::ItemFold(f) => f.text.len(),
        _ => 0,
    }
}

fn buffered_bytes(state: &Channel) -> usize {
    state.waiting_streams.iter().map(body_bytes).sum()
}

/// Full jitter bounded exponential backoff; randomness is supplied by the driver.
pub fn reconnect_delay_ms(attempt: u32, random: u64) -> u64 {
    let cap = (500_u64.saturating_mul(1_u64 << attempt.min(6))).min(30_000);
    100 + random % cap
}

#[cfg(test)]
mod tests;
