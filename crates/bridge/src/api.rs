use flutter_rust_bridge::frb;
use prost::Message;
use ryzhub_core::Engine;
use ryzhub_proto::{command_envelope::Command, frame::Body, *};
use serde_json::json;
use std::{collections::BTreeMap, sync::Mutex};

pub struct ClientCore {
    inner: Mutex<Engine>,
}
pub struct Update {
    pub changes_json: String,
    pub cache_json: Option<String>,
    pub outbound_frames: Vec<Vec<u8>>,
    pub error: Option<String>,
}

impl ClientCore {
    #[frb(sync)]
    pub fn new(cache_json: String) -> Result<Self, String> {
        let engine = Engine::restore(&cache_json).map_err(|e| e.to_string())?;
        Ok(Self {
            inner: Mutex::new(engine),
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
                resume_token: self.inner.lock().unwrap().resume_token.clone(),
            }),
        )
        .encode_to_vec()
    }
    #[frb(sync)]
    pub fn view_json(&self) -> String {
        serde_json::to_string(&*self.inner.lock().unwrap()).unwrap()
    }
    #[frb(sync)]
    pub fn merge_history(&self, channel: String, items_json: String) -> Result<Update, String> {
        let items = serde_json::from_str(&items_json).map_err(|e| e.to_string())?;
        let mut engine = self.inner.lock().unwrap();
        let changed = engine.merge_history(&channel, items);
        let changes = match engine.channels.get(&channel) {
            Some(state) => json!([{"channel":channel,"items":changed,
                "actions":state.actions.values().collect::<Vec<_>>(),
                "runs":state.runs.values().collect::<Vec<_>>(),"cursor":state.cursor}]),
            None => json!([]),
        };
        Ok(Update {
            changes_json: changes.to_string(),
            cache_json: Some(serde_json::to_string(&*engine).unwrap()),
            outbound_frames: vec![],
            error: None,
        })
    }
    #[frb(sync)]
    pub fn subscribe(&self, channel: String) -> Update {
        let mut engine = self.inner.lock().unwrap();
        let mut outbound = Vec::new();
        if channel.starts_with("conv:") {
            let old: Vec<_> = engine
                .channels
                .iter()
                .filter(|(c, s)| c.starts_with("conv:") && **c != channel && s.subscribed)
                .map(|(c, _)| c.clone())
                .collect();
            for c in old {
                outbound.push(engine.unsubscribe(c).encode_to_vec());
            }
        }
        outbound.push(engine.subscribe(channel).encode_to_vec());
        Update {
            changes_json: "[]".into(),
            cache_json: Some(serde_json::to_string(&*engine).unwrap()),
            outbound_frames: outbound,
            error: None,
        }
    }
    #[frb(sync)]
    /// `config_json` carries settings not yet confirmed by the server, so they
    /// apply to this message's run regardless of command ordering.
    pub fn send_text(&self, channel: String, text: String, config_json: Option<String>) -> Update {
        self.command(
            channel,
            Command::SendMessage(SendMessage {
                text,
                config_json: config_json.unwrap_or_default(),
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
    pub fn steer(&self, channel: String, text: String) -> Update {
        self.command(channel, Command::Steer(Steer { text }))
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
        let mut engine = self.inner.lock().unwrap();
        let f = engine.enqueue(
            channel,
            CommandEnvelope {
                cid: uuid::Uuid::now_v7().to_string(),
                command: Some(command),
            },
        );
        Update {
            changes_json: "[]".into(),
            cache_json: Some(serde_json::to_string(&*engine).unwrap()),
            outbound_frames: vec![f.encode_to_vec()],
            error: None,
        }
    }
    #[frb(sync)]
    pub fn retry_pending(&self) -> Vec<Vec<u8>> {
        self.inner
            .lock()
            .unwrap()
            .pending_commands
            .values()
            .map(|f| f.encode_to_vec())
            .collect()
    }
    /// Flutter batches incoming frames for one display tick, then receives item diffs.
    #[frb(sync)]
    pub fn ingest(&self, frames: Vec<Vec<u8>>) -> Result<Update, String> {
        let mut engine = self.inner.lock().unwrap();
        let before: BTreeMap<_, _> = engine
            .channels
            .iter()
            .map(|(c, s)| {
                (
                    c.clone(),
                    s.items
                        .iter()
                        .map(|(id, i)| (id.clone(), (i.revision, i.text.len())))
                        .collect::<BTreeMap<_, _>>(),
                )
            })
            .collect();
        let mut outbound_frames = vec![];
        let mut durable = false;
        let mut reset = std::collections::BTreeSet::new();
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
            if matches!(f.body, Some(Body::Snapshot(_))) {
                reset.insert(f.channel.clone());
            }
            if matches!(f.body, Some(Body::Welcome(_))) {
                outbound_frames.extend(engine.reconnect().into_iter().map(|f| f.encode_to_vec()));
            }
            outbound_frames.extend(
                engine
                    .apply(f)
                    .outbound
                    .into_iter()
                    .map(|f| f.encode_to_vec()),
            );
        }
        let mut changes = vec![];
        for (channel, state) in &engine.channels {
            let items: Vec<_> = state
                .items
                .values()
                .filter(|i| {
                    reset.contains(channel)
                        || before.get(channel).and_then(|b| b.get(&i.id))
                            != Some(&(i.revision, i.text.len()))
                })
                .collect();
            changes.push(json!({"channel":channel,"reset":reset.contains(channel),"items":items,"actions":state.actions.values().collect::<Vec<_>>(),"runs":state.runs.values().collect::<Vec<_>>(),"cursor":state.cursor}));
        }
        for channel in before.keys().filter(|c| !engine.channels.contains_key(*c)) {
            changes.push(json!({"channel":channel,"revoked":true}));
        }
        Ok(Update {
            changes_json: serde_json::to_string(&changes).unwrap(),
            cache_json: durable.then(|| serde_json::to_string(&*engine).unwrap()),
            outbound_frames,
            error: engine.last_error.take(),
        })
    }
}
#[frb(sync)]
pub fn reconnect_delay(attempt: u32, random: u32) -> u32 {
    ryzhub_core::reconnect_delay_ms(attempt, random as u64) as u32
}
