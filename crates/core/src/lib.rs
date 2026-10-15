//! Deterministic client projection. No clocks, sockets, filesystem, or platform APIs.

pub mod ansi;
pub mod diff;
pub mod highlight;
pub mod markdown;
pub mod timeline;
use ryzhub_proto::{command_envelope::Command, event_envelope::Event, frame::Body, *};
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
    /// Unsent messages by conversation, in a user channel.
    pub drafts: BTreeMap<String, Draft>,
    /// Output so far of running commands, by item, with the item revision it streams on.
    /// Not cached: the finished command's item carries its output.
    #[serde(skip)]
    pub outputs: BTreeMap<String, (u64, String)>,
    #[serde(skip)]
    waiting_events: BTreeMap<u64, EventEnvelope>,
    #[serde(skip)]
    waiting_streams: Vec<Body>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Engine {
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
    pending_commands: &'a BTreeMap<String, Frame>,
    schema: u32,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Meta {
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
            pending_commands: meta.pending_commands,
            schema: meta.schema,
            ..Default::default()
        };
        for (name, json) in channels {
            engine.channels.insert(name, serde_json::from_str(&json)?);
        }
        Ok(engine.upgraded())
    }

    /// What persists beside the channels: the commands not yet acknowledged.
    pub fn meta_json(&self) -> String {
        serde_json::to_string(&MetaRef {
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
        // A new connection has no subscriptions: only the subscribed channels are named.
        let channels: Vec<_> = self
            .channels
            .iter()
            .filter(|(_, s)| s.subscribed)
            .map(|(c, _)| c.clone())
            .collect();
        let mut frames: Vec<_> = channels.into_iter().map(|c| self.subscribe(c)).collect();
        frames.extend(self.pending_commands.values().cloned());
        frames
    }

    /// Whether `frame` waits for its conversation to be created: a pending CreateRun is
    /// creating it, and the server knows no such channel until it is done.
    pub fn held(&self, frame: &Frame) -> bool {
        frame.channel.starts_with("conv:")
            && self
                .pending_commands
                .values()
                .filter_map(created_channel)
                .any(|channel| channel == frame.channel)
    }

    /// Once its CreateRun is answered, a conversation's held frames go out: its subscription
    /// and the commands queued for it.
    fn release(&mut self, channel: &str, effects: &mut Effects) {
        if self.channels.get(channel).is_some_and(|s| s.subscribed) {
            effects.outbound.push(self.subscribe(channel.to_owned()));
        }
        effects.outbound.extend(
            self.pending_commands
                .values()
                .filter(|f| f.channel == channel)
                .cloned(),
        );
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
            Body::Welcome(_) => {}
            Body::Ping(p) => effects
                .outbound
                .push(frame("", Body::Pong(Pong { nonce: p.nonce }))),
            Body::CommandAck(a) => {
                let removed = self.pending_commands.remove(&a.cid);
                if let Some(created) = removed.as_ref().and_then(created_channel) {
                    self.release(&created, &mut effects);
                }
                let channel = removed.map_or(input.channel, |f| f.channel);
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
                if !r.retryable
                    && let Some(removed) = self.pending_commands.remove(&r.cid)
                    && let Some(created) = created_channel(&removed)
                {
                    // Refused, the conversation may not exist: its held commands are refused in
                    // turn, and a subscription to it revoked.
                    self.release(&created, &mut effects);
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
                            state.drafts = s
                                .drafts
                                .into_iter()
                                .map(|d| (d.conversation_id.clone(), d))
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
        Some(Event::DraftChanged(draft)) => {
            state.drafts.insert(draft.conversation_id.clone(), draft);
        }
        None => {}
    }
    None
}

/// The conversation channel a CreateRun command creates.
fn created_channel(frame: &Frame) -> Option<String> {
    match &frame.body {
        Some(Body::Command(CommandEnvelope {
            command: Some(Command::CreateRun(create)),
            ..
        })) => Some(format!("conv:{}", create.conversation_id)),
        _ => None,
    }
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
    // A reply's text, or a command's output; other streams do not corrupt either.
    let output = match stream.as_str() {
        "text" => false,
        "terminal_output" => true,
        _ => return StreamResult::Ignored,
    };
    if part != 0 {
        return StreamResult::Ignored;
    }
    if base > state.cursor {
        return StreamResult::Future;
    }
    let Some(item) = state.items.get_mut(id) else {
        return StreamResult::Gap;
    };
    let open = if output {
        matches!(item.state.as_str(), "streaming" | "running" | "planned")
    } else {
        item.state == "streaming"
    };
    if !open || revision < item.revision || base < item.last_seq {
        return StreamResult::Ignored;
    }
    if revision > item.revision || base > item.last_seq {
        return StreamResult::Gap;
    }
    let target = if output {
        let entry = state
            .outputs
            .entry(id.clone())
            .or_insert_with(|| (revision, String::new()));
        if entry.0 != revision {
            *entry = (revision, String::new());
        }
        &mut entry.1
    } else {
        &mut item.text
    };
    append(target, text, offset)
}

/// Applies streamed `text` to `target`: the whole text so far (a fold), or what follows byte
/// `offset` (a delta), whose overlap with what is there must agree.
fn append(target: &mut String, text: &str, offset: Option<u64>) -> StreamResult {
    let Some(offset) = offset else {
        if text.len() <= target.len() {
            return StreamResult::Ignored;
        }
        if !text.starts_with(target.as_str()) {
            return StreamResult::Gap;
        }
        *target = text.to_owned();
        return StreamResult::Applied;
    };
    let Ok(offset) = usize::try_from(offset) else {
        return StreamResult::Gap;
    };
    if offset > target.len() || !target.is_char_boundary(offset) {
        return StreamResult::Gap;
    }
    let overlap = (target.len() - offset).min(text.len());
    if target.as_bytes()[offset..offset + overlap] != text.as_bytes()[..overlap]
        || !text.is_char_boundary(overlap)
    {
        return StreamResult::Gap;
    }
    target.push_str(&text[overlap..]);
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

/// zstd's frame magic number. A protobuf frame never starts with it: its first byte would be
/// the key of field 5, which frames do not have.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
/// Largest frame a compressed message may expand to.
const MAX_FRAME_BYTES: u64 = 4 * 1024 * 1024;

/// The frame a server message carries: as it is, or decompressed when the server sent it as a
/// zstd frame (`ryzhub.v1.protobuf+zstd`).
pub fn frame_bytes(message: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>, String> {
    use std::io::Read;
    if !message.starts_with(&ZSTD_MAGIC) {
        return Ok(std::borrow::Cow::Borrowed(message));
    }
    let decoder = ruzstd::decoding::StreamingDecoder::new(message).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    decoder
        .take(MAX_FRAME_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    if out.len() as u64 > MAX_FRAME_BYTES {
        return Err("compressed frame too large".into());
    }
    Ok(std::borrow::Cow::Owned(out))
}

/// Full jitter bounded exponential backoff; randomness is supplied by the driver.
pub fn reconnect_delay_ms(attempt: u32, random: u64) -> u64 {
    let cap = (500_u64.saturating_mul(1_u64 << attempt.min(6))).min(30_000);
    100 + random % cap
}

#[cfg(test)]
mod tests;
