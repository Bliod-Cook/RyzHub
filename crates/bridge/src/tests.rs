use crate::api::{CacheRecord, CacheWrite, ClientCore};
use prost::Message;
use ryzhub_proto::{event_envelope::Event, frame::Body, *};

fn bytes(frame: Frame) -> Vec<u8> {
    frame.encode_to_vec()
}

fn event(channel: &str, seq: u64, id: &str) -> Vec<u8> {
    bytes(frame(
        channel,
        Body::Event(EventEnvelope {
            seq,
            cause_cid: "".into(),
            event: Some(Event::ItemUpserted(Item {
                id: id.into(),
                revision: seq,
                kind: "assistant".into(),
                state: "completed".into(),
                text: format!("text {seq}"),
                first_seq: seq,
                last_seq: seq,
                ..Default::default()
            })),
        }),
    ))
}

fn keys(writes: &[CacheWrite]) -> Vec<(&str, bool)> {
    writes
        .iter()
        .map(|write| (write.key.as_str(), write.value.is_some()))
        .collect()
}

/// A client subscribed to `count` conversations, its cache stored.
fn client(count: usize) -> ClientCore {
    let core = ClientCore::new(vec![]).unwrap();
    for n in 0..count {
        core.subscribe(format!("conv:{n}"));
        core.committed();
    }
    core
}

#[test]
fn an_update_carries_and_stores_only_what_it_touched() {
    let core = client(20);
    let update = core.ingest(vec![event("conv:7", 1, "a")]).unwrap();
    let changes: serde_json::Value = serde_json::from_str(&update.changes_json).unwrap();
    let changes = changes.as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["channel"], "conv:7");
    assert_eq!(changes[0]["items"][0]["id"], "a");
    assert_eq!(
        keys(&update.cache_writes),
        [("engine", true), ("channel:conv:7", true)]
    );
    core.committed();
    // Streamed text is not stored by itself; the next durable update stores its channel.
    let delta = bytes(frame(
        "conv:3",
        Body::Delta(Delta {
            item_id: "missing".into(),
            stream: "text".into(),
            ..Default::default()
        }),
    ));
    assert!(core.ingest(vec![delta]).unwrap().cache_writes.is_empty());
    let update = core.ingest(vec![event("conv:7", 2, "b")]).unwrap();
    assert_eq!(
        keys(&update.cache_writes),
        [("engine", true), ("channel:conv:7", true)]
    );
}

#[test]
fn a_command_is_sent_again_only_once_stored() {
    let core = client(1);
    let queued = core.send_text("conv:0".into(), "hi".into(), None, vec![]);
    let cid = queued.cid.clone().unwrap();
    assert_eq!(queued.outbound_frames.len(), 1);
    // The cache write failed: neither the timer nor a reconnect may send it.
    assert!(core.retry_pending().is_empty());
    let welcome = bytes(frame("", Body::Welcome(Welcome::default())));
    let reconnected = core.ingest(vec![welcome.clone()]).unwrap();
    assert!(
        reconnected.outbound_frames.iter().all(|f| !matches!(
            Frame::decode(f.as_slice()).unwrap().body,
            Some(Body::Command(_))
        )),
        "an unstored command went out on reconnect"
    );
    // Stored with a later save, it goes out again.
    core.committed();
    assert_eq!(core.retry_pending().len(), 1);
    let ack = bytes(frame(
        "conv:0",
        Body::CommandAck(CommandAck {
            cid: cid.clone(),
            ..Default::default()
        }),
    ));
    let acked = core.ingest(vec![ack]).unwrap();
    let results: serde_json::Value = serde_json::from_str(&acked.results_json).unwrap();
    assert_eq!(results[0]["cid"], cid.as_str());
    assert_eq!(results[0]["channel"], "conv:0");
    assert_eq!(results[0]["accepted"], true);
    core.committed();
    assert!(core.retry_pending().is_empty());
}

#[test]
fn refusals_name_their_command_and_expiry_asks_for_a_refresh() {
    let core = client(1);
    let cid = core
        .send_text("conv:0".into(), "hi".into(), None, vec![])
        .cid
        .unwrap();
    core.committed();
    let refused = bytes(frame(
        "conv:0",
        Body::CommandRejected(CommandRejected {
            cid: cid.clone(),
            code: "ATTACHMENT_UNAVAILABLE".into(),
            retryable: false,
        }),
    ));
    let expiring = bytes(frame(
        "",
        Body::AuthExpiring(AuthExpiring { expires_at_ms: 9 }),
    ));
    let update = core.ingest(vec![refused, expiring]).unwrap();
    let results: serde_json::Value = serde_json::from_str(&update.results_json).unwrap();
    assert_eq!(results[0]["accepted"], false);
    assert_eq!(results[0]["code"], "ATTACHMENT_UNAVAILABLE");
    assert_eq!(results[0]["channel"], "conv:0");
    assert!(update.auth_expiring);
    assert!(
        update.error.is_none(),
        "refusals are results, not a bare error"
    );
}

#[test]
fn a_cache_from_before_records_becomes_records_keeping_its_commands() {
    let old = client(3);
    old.send_text("conv:1".into(), "queued".into(), None, vec![]);
    let legacy = old.view_json();
    let core = ClientCore::new(vec![CacheRecord {
        key: String::new(),
        value: legacy,
    }])
    .unwrap();
    // Read back from before, the command may go out again.
    assert_eq!(core.retry_pending().len(), 1);
    let update = core.subscribe("user:o:u".into());
    let mut written = keys(&update.cache_writes);
    written.sort();
    assert_eq!(
        written,
        [
            ("", false),
            ("channel:conv:0", true),
            ("channel:conv:1", true),
            ("channel:conv:2", true),
            ("channel:user:o:u", true),
            ("engine", true)
        ]
    );
    core.committed();
    let records: Vec<CacheRecord> = update
        .cache_writes
        .into_iter()
        .filter_map(|write| {
            Some(CacheRecord {
                key: write.key,
                value: write.value?,
            })
        })
        .collect();
    let reopened = ClientCore::new(records).unwrap();
    assert_eq!(reopened.retry_pending().len(), 1);
    let view: serde_json::Value = serde_json::from_str(&reopened.view_json()).unwrap();
    assert_eq!(view["channels"].as_object().unwrap().len(), 4);
    // Once stored, the record from before is not removed again.
    let next = core.subscribe("conv:0".into());
    assert!(next.cache_writes.iter().all(|write| !write.key.is_empty()));
}
