use super::*;

#[test]
fn paged_history_preserves_streams_revisions_cursor_and_revocation() {
    let mut e = engine();
    e.apply(event(1, 1, "", "streaming"));
    e.apply(delta(1, 0, "live text"));
    let mut stale = e.channels["conv:x"].items["i"].clone();
    stale.text.clear();
    let page: Vec<_> = (0..150)
        .map(|n| Item {
            id: format!("history-{n}"),
            first_seq: n + 1,
            revision: 1,
            state: "completed".into(),
            text: format!("message {n}"),
            ..Default::default()
        })
        .collect();
    for batch in page.chunks(100) {
        e.merge_history("conv:x", batch.to_vec());
    }
    assert!(e.merge_history("conv:x", vec![stale.clone()]).is_empty());
    assert_eq!(content(&e), "live text");
    assert_eq!(e.channels["conv:x"].cursor, 1);
    assert_eq!(e.channels["conv:x"].items.len(), 151);
    e.apply(event(2, 2, "final", "completed"));
    e.merge_history("conv:x", vec![stale.clone()]);
    assert_eq!(content(&e), "final");
    let cache = serde_json::to_string(&e).unwrap();
    let restored: Engine = serde_json::from_str(&cache).unwrap();
    assert_eq!(restored.channels["conv:x"].items.len(), 151);
    e.apply(frame(
        "conv:x",
        Body::SubscriptionRevoked(SubscriptionRevoked::default()),
    ));
    e.merge_history("conv:x", vec![stale]);
    assert!(!e.channels.contains_key("conv:x"));
}

fn engine() -> Engine {
    let mut e = Engine::default();
    e.subscribe("conv:x".into());
    e
}
fn event(seq: u64, revision: u64, text: &str, state: &str) -> Frame {
    frame(
        "conv:x",
        Body::Event(EventEnvelope {
            seq,
            cause_cid: "".into(),
            event: Some(Event::ItemUpserted(Item {
                id: "i".into(),
                revision,
                kind: "assistant".into(),
                state: state.into(),
                text: text.into(),
                first_seq: 1,
                last_seq: seq,
                run_id: "r".into(),
            })),
        }),
    )
}
fn delta(base: u64, offset: u64, text: &str) -> Frame {
    frame(
        "conv:x",
        Body::Delta(Delta {
            item_id: "i".into(),
            stream: "text".into(),
            part_index: 0,
            offset,
            text: text.into(),
            base_seq: base,
            revision: 1,
        }),
    )
}
fn fold(text: &str) -> Frame {
    frame(
        "conv:x",
        Body::ItemFold(ItemFold {
            item_id: "i".into(),
            stream: "text".into(),
            part_index: 0,
            text: text.into(),
            base_seq: 1,
            revision: 1,
        }),
    )
}
fn content(e: &Engine) -> &str {
    &e.channels["conv:x"].items["i"].text
}

#[test]
fn duplicate_partial_overlap_and_unicode_offsets() {
    let mut e = engine();
    e.apply(event(1, 1, "", "streaming"));
    e.apply(delta(1, 0, "你好"));
    e.apply(delta(1, 0, "你好"));
    e.apply(delta(1, 3, "好🦀"));
    assert_eq!(content(&e), "你好🦀");
    assert_eq!(e.channels["conv:x"].cursor, 1);
    assert_eq!(e.apply(delta(1, 1, "invalid")).outbound.len(), 1);
}
#[test]
fn delta_before_item_waits_for_base_seq() {
    let mut e = engine();
    e.apply(delta(1, 0, "first"));
    assert!(e.channels["conv:x"].items.is_empty());
    e.apply(event(1, 1, "", "streaming"));
    assert_eq!(content(&e), "first");
}
#[test]
fn gaps_request_resubscription_and_old_folds_never_truncate() {
    let mut e = engine();
    e.apply(event(1, 1, "", "streaming"));
    assert_eq!(e.apply(delta(1, 5, "lost")).outbound.len(), 1);
    e.apply(fold("complete fold"));
    e.apply(fold("complete"));
    assert_eq!(content(&e), "complete fold");
    e.apply(event(2, 2, "final", "completed"));
    e.apply(fold("complete fold later"));
    assert_eq!(content(&e), "final");
}
#[test]
fn cursor_only_advances_over_contiguous_events() {
    let mut e = engine();
    e.apply(event(3, 3, "three", "completed"));
    assert_eq!(e.channels["conv:x"].cursor, 0);
    e.apply(event(1, 1, "one", "streaming"));
    e.apply(event(2, 2, "two", "streaming"));
    assert_eq!(e.channels["conv:x"].cursor, 3);
    assert_eq!(content(&e), "three");
    e.apply(event(2, 2, "two", "streaming"));
    assert_eq!(content(&e), "three");
}
#[test]
fn lost_ack_retries_same_command_after_serialization() {
    let mut e = engine();
    let first = e.enqueue(
        "conv:x".into(),
        CommandEnvelope {
            cid: "stable-cid".into(),
            command: Some(command_envelope::Command::SendMessage(SendMessage {
                text: "hi".into(),
            })),
        },
    );
    let mut restored: Engine = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    assert!(restored.reconnect().contains(&first));
    restored.apply(frame(
        "conv:x",
        Body::CommandAck(CommandAck {
            cid: "stable-cid".into(),
            accepted_seq: 1,
            result: "accepted".into(),
        }),
    ));
    assert!(restored.pending_commands.is_empty());
}
#[test]
fn snapshot_then_live_event_has_no_gap_and_old_snapshot_is_ignored() {
    let mut e = engine();
    e.apply(frame(
        "conv:x",
        Body::Snapshot(Snapshot {
            head_seq: 20,
            items: vec![],
            pending_actions: vec![],
            ..Default::default()
        }),
    ));
    e.apply(event(21, 1, "after snapshot", "completed"));
    e.apply(frame(
        "conv:x",
        Body::Snapshot(Snapshot {
            head_seq: 10,
            items: vec![],
            pending_actions: vec![],
            ..Default::default()
        }),
    ));
    assert_eq!(e.channels["conv:x"].cursor, 21);
    assert_eq!(content(&e), "after snapshot");
}
#[test]
fn revoke_clears_private_projection_and_pending_commands() {
    let mut e = engine();
    e.apply(event(1, 1, "private", "completed"));
    e.enqueue(
        "conv:x".into(),
        CommandEnvelope {
            cid: "c".into(),
            command: None,
        },
    );
    e.apply(frame(
        "conv:x",
        Body::SubscriptionRevoked(SubscriptionRevoked {
            reason: "removed".into(),
        }),
    ));
    assert!(e.channels.is_empty());
    assert!(e.pending_commands.is_empty());
}
#[test]
fn excessive_future_deltas_request_fold_without_unbounded_buffering() {
    let mut e = engine();
    for _ in 0..MAX_BUFFERED {
        e.apply(delta(1, 0, "x"));
    }
    assert_eq!(e.apply(delta(1, 0, "x")).outbound.len(), 1);
    assert!(e.channels["conv:x"].waiting_streams.is_empty());
}

#[test]
fn subscription_diff_keeps_cache_but_removes_inactive_resume_subscriptions() {
    let mut e = engine();
    e.apply(event(1, 1, "cached", "completed"));
    e.unsubscribe("conv:x".into());
    let frames = e.reconnect();
    assert!(
        frames
            .iter()
            .any(|f| f.channel == "conv:x" && matches!(f.body, Some(Body::Unsubscribe(_))))
    );
    assert!(
        !frames
            .iter()
            .any(|f| f.channel == "conv:x" && matches!(f.body, Some(Body::Subscribe(_))))
    );
    assert_eq!(content(&e), "cached");
    assert!(
        e.apply(frame(
            "conv:x",
            Body::ChannelLagged(ChannelLagged {
                reason: "old subscription".into()
            })
        ))
        .outbound
        .is_empty()
    );
}
