//! Projection of harness events onto the conversation timeline.
//!
//! Guest events are applied in `gseq` order, each in the same transaction that advances the
//! acknowledged position, so a crash replays them exactly once. An event belongs to the run
//! that registered its native turn; an event whose turn is not registered yet stays
//! unacknowledged until it is.
use anyhow::{Context, ensure};
use ryzhub_execution::{
    GuestCommand, GuestEvent, HarnessAnswer, HarnessEvent,
    harness::{ApprovalCategory, ItemContent, ItemKind, ItemPhase, TurnStatus},
};
use ryzhub_proto::{
    FileChange, FileChangeDetail, Item, ItemDetail, SharedFile, ToolDetail, item_detail,
};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    app::App,
    attachments::outputs,
    error::Code,
    owner::{LiveRef, OwnerLink, WorkerEvent},
    runs, sandbox,
    store::{Lease, Tx, load_item, upsert_item},
};

/// Apply a replay reply. Returns true when `run`'s turn has finished.
pub async fn apply(
    app: &App,
    lease: &Lease,
    run: Uuid,
    sandbox_id: Uuid,
    reply: &Value,
    through: Option<u64>,
    link: &OwnerLink,
) -> anyhow::Result<bool> {
    let events: Vec<GuestEvent> = serde_json::from_value(reply["events"].clone())?;
    // Files the finishing turn's replies link to are copied out first, with no transaction
    // open; a crash before the commit below publishes them again, finding the same entries.
    let shared = hand_over(app, lease, run, sandbox_id, &events).await?;
    let mut tx = app.store.fenced(lease).await?;
    let mut current: i64 =
        sqlx::query_scalar("SELECT acked_gseq FROM sandboxes WHERE org_id=$1 AND id=$2 FOR UPDATE")
            .bind(lease.org_id)
            .bind(sandbox_id)
            .fetch_one(&mut *tx)
            .await?;
    let mut done = false;
    let acked = current as u64;
    for event in events.into_iter().filter(|e| e.gseq > acked) {
        if through.is_some_and(|through| event.gseq > through) {
            break;
        }
        ensure!(event.gseq == current as u64 + 1, "guest event gap");
        let mut owner = run;
        if event.op_id == "native" {
            let harness: HarnessEvent = serde_json::from_value(event.result.clone())?;
            if let HarnessEvent::TurnStarted { turn_id } = &harness {
                // A native compaction has no turn id until the harness starts it.
                sqlx::query(
                    "UPDATE runs SET native_turn_id=$3 WHERE org_id=$1 AND id=$2 AND operation='compact'
                     AND native_turn_id IS NULL AND state='running'",
                )
                .bind(lease.org_id)
                .bind(run)
                .bind(turn_id)
                .execute(&mut *tx)
                .await?;
            }
            if routed(&harness)
                && let Some(turn) = harness.turn_id()
            {
                // turn/start may reply before its id is committed: wait for registration.
                let Some(target) = turn_owner(&mut tx, lease, sandbox_id, turn).await? else {
                    break;
                };
                owner = target;
            }
            let finished = project(
                &mut tx,
                lease,
                owner,
                sandbox_id,
                &harness,
                &app.config,
                &shared,
            )
            .await?;
            // Recorded after projection: its channel position covers what this event wrote.
            record(&mut tx, lease, sandbox_id, &event).await?;
            done |= owner == run && finished;
        } else {
            record(&mut tx, lease, sandbox_id, &event).await?;
        }
        current = event.gseq as i64;
    }
    sqlx::query(
        "UPDATE sandboxes SET acked_gseq=$3,node_lease_until=now()+interval '15 seconds' WHERE org_id=$1 AND id=$2",
    )
    .bind(lease.org_id)
    .bind(sandbox_id)
    .bind(current)
    .execute(&mut *tx)
    .await?;
    let streaming = if done || through.is_some() {
        vec![]
    } else {
        streaming_text(&mut tx, lease, sandbox_id, reply).await?
    };
    tx.commit().await?;
    link.committed().await;
    sandbox::guest(
        app,
        lease,
        sandbox_id,
        GuestCommand::Ack {
            through: current as u64,
        },
    )
    .await?;
    for (item, stream, text) in streaming {
        link.send(WorkerEvent::Replace { item, stream, text })
            .await?;
    }
    Ok(done)
}

/// Events that belong to a specific run's turn.
fn routed(event: &HarnessEvent) -> bool {
    matches!(
        event,
        HarnessEvent::Item { .. }
            | HarnessEvent::TurnCompleted { .. }
            | HarnessEvent::ApprovalRequested { .. }
            | HarnessEvent::QuestionRequested { .. }
    )
}

async fn turn_owner(
    tx: &mut Tx<'_>,
    lease: &Lease,
    sandbox: Uuid,
    turn: &str,
) -> anyhow::Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2 AND sandbox_id=$3
         AND (native_turn_id=$4 OR EXISTS(SELECT 1 FROM native_turns nt WHERE nt.org_id=runs.org_id
              AND nt.sandbox_id=$3 AND nt.turn_id=$4 AND nt.run_id=runs.id))",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(sandbox)
    .bind(turn)
    .fetch_optional(&mut **tx)
    .await?)
}

/// Keep the event, with the channel position it was projected at (the checkpoint watermark).
async fn record(
    tx: &mut Tx<'_>,
    lease: &Lease,
    sandbox: Uuid,
    event: &GuestEvent,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO guest_events(org_id,sandbox_id,gseq,payload,channel_seq)
         VALUES($1,$2,$3,$4,(SELECT last_seq FROM channels WHERE org_id=$1 AND conversation_id=$5))
         ON CONFLICT DO NOTHING",
    )
    .bind(lease.org_id)
    .bind(sandbox)
    .bind(event.gseq as i64)
    .bind(serde_json::to_value(event)?)
    .bind(lease.conversation_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Current text of streaming replies and output of running commands, for the Owner's live
/// streams.
async fn streaming_text(
    tx: &mut Tx<'_>,
    lease: &Lease,
    sandbox: Uuid,
    reply: &Value,
) -> anyhow::Result<Vec<(LiveRef, &'static str, String)>> {
    let mut streaming = vec![];
    let folds = reply["folds"].as_object().into_iter().flatten();
    let outputs = reply["outputs"].as_object().into_iter().flatten();
    for (stream, (native, text)) in folds
        .map(|f| (crate::owner::TEXT, f))
        .chain(outputs.map(|o| (crate::owner::TERMINAL_OUTPUT, o)))
    {
        let item: Option<Uuid> = sqlx::query_scalar(
            "SELECT item_id FROM native_items WHERE org_id=$1 AND sandbox_id=$2 AND native_id=$3",
        )
        .bind(lease.org_id)
        .bind(sandbox)
        .bind(native)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(item) = item else { continue };
        let item = load_item(tx, lease.org_id, item).await?;
        let kind = if stream == crate::owner::TEXT {
            "assistant"
        } else {
            "tool"
        };
        if item.state == "streaming" && item.kind == kind {
            streaming.push((
                LiveRef::of(&item),
                stream,
                text.as_str().unwrap_or_default().to_owned(),
            ));
        }
    }
    Ok(streaming)
}

/// Apply one harness event to `run`. Returns true when the run's turn finished.
async fn project(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    sandbox: Uuid,
    event: &HarnessEvent,
    config: &crate::config::Config,
    shared: &[SharedFile],
) -> anyhow::Result<bool> {
    if let Some(turn) = event.turn_id() {
        let current: Option<String> =
            sqlx::query_scalar("SELECT native_turn_id FROM runs WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(run)
                .fetch_one(&mut **tx)
                .await?;
        if current.as_deref().is_some_and(|current| current != turn) {
            let known: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM native_turns WHERE org_id=$1 AND sandbox_id=$2 AND turn_id=$3 AND run_id=$4)",
            )
            .bind(lease.org_id)
            .bind(sandbox)
            .bind(turn)
            .bind(run)
            .fetch_one(&mut **tx)
            .await?;
            if !known {
                return Ok(false);
            }
        }
    }
    match event {
        // A report without models (Codex's skills) keeps the models listed before.
        HarnessEvent::Capabilities { commands, models } => {
            sqlx::query(
                "UPDATE sandboxes SET commands=$3,capabilities=CASE WHEN $5 THEN capabilities
                   ELSE jsonb_set(capabilities,'{models}',$4) END
                 WHERE org_id=$1 AND id=$2",
            )
            .bind(lease.org_id)
            .bind(sandbox)
            .bind(commands)
            .bind(serde_json::to_value(models)?)
            .bind(models.is_empty())
            .execute(&mut **tx)
            .await?;
        }
        HarnessEvent::ApprovalRequested {
            request_id,
            category,
            summary,
            ..
        } => {
            let settings: Value =
                sqlx::query_scalar("SELECT run_config FROM runs WHERE org_id=$1 AND id=$2")
                    .bind(lease.org_id)
                    .bind(run)
                    .fetch_one(&mut **tx)
                    .await?;
            let automatic = *category == ApprovalCategory::FileChange
                && settings["approval"] == "allow_edits"
                && settings["plan"] != true;
            let answer = automatic.then_some(HarnessAnswer::Approval { allow: true });
            request(
                tx, lease, run, sandbox, request_id, event, "approval", summary, answer, config,
            )
            .await?;
        }
        HarnessEvent::QuestionRequested {
            request_id,
            questions,
            ..
        } => {
            let summary = serde_json::to_string(questions)?;
            request(
                tx, lease, run, sandbox, request_id, event, "question", &summary, None, config,
            )
            .await?;
        }
        HarnessEvent::Item {
            phase,
            native_id,
            kind,
            content,
            message_id,
            parent_native_id,
            ..
        } => {
            project_item(
                tx,
                lease,
                run,
                sandbox,
                Native {
                    phase: *phase,
                    id: native_id,
                    kind: *kind,
                    content,
                    message_id: message_id.as_deref(),
                    parent: parent_native_id.as_deref(),
                },
            )
            .await?
        }
        HarnessEvent::TurnCompleted { status, .. } => {
            // Harnesses without native steering (all but Codex) queue steers into a following
            // native turn of the same platform run.
            let queued: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM runs r JOIN pending_inputs i ON i.org_id=r.org_id AND i.run_id=r.id
                 WHERE r.org_id=$1 AND r.id=$2 AND r.harness<>'codex' AND r.state='running' AND i.consumed_step IS NULL)",
            )
            .bind(lease.org_id)
            .bind(run)
            .fetch_one(&mut **tx)
            .await?;
            if queued && *status == TurnStatus::Completed {
                sqlx::query("UPDATE runs SET native_turn_id=NULL WHERE org_id=$1 AND id=$2")
                    .bind(lease.org_id)
                    .bind(run)
                    .execute(&mut **tx)
                    .await?;
                return Ok(false);
            }
            let (state, error) = match status {
                TurnStatus::Completed => ("completed", None),
                TurnStatus::Interrupted => ("cancelled", None),
                TurnStatus::Failed => ("failed", Some(Code::NativeTurnFailed)),
            };
            if *status == TurnStatus::Completed {
                // After the replies' last updates, which would clear what is recorded here.
                outputs::record(tx, lease, run, shared).await?;
            }
            runs::finish(tx, lease, run, state, error).await?;
            return Ok(true);
        }
        // The turn ended with the harness, whose exit is acknowledged with it: the guest starts
        // a fresh harness for the next run.
        HarnessEvent::Exited { .. } => {
            runs::finish(tx, lease, run, "failed", Some(Code::NativeOutcomeUnknown)).await?;
            return Ok(true);
        }
        HarnessEvent::TurnStarted { .. } => {}
    }
    Ok(false)
}

/// Publish the files the replies of `run`'s finishing turn link to, when `events` complete it.
async fn hand_over(
    app: &App,
    lease: &Lease,
    run: Uuid,
    sandbox: Uuid,
    events: &[GuestEvent],
) -> anyhow::Result<Vec<SharedFile>> {
    let mut finishing = None;
    let mut texts = Vec::new();
    for event in events.iter().filter(|event| event.op_id == "native") {
        let Ok(harness) = serde_json::from_value::<HarnessEvent>(event.result.clone()) else {
            continue;
        };
        match harness {
            HarnessEvent::TurnCompleted {
                turn_id,
                status: TurnStatus::Completed,
            } => finishing = Some(turn_id),
            HarnessEvent::Item {
                phase: ItemPhase::Completed,
                kind: ItemKind::Assistant,
                content,
                ..
            } => texts.push(content.text),
            _ => {}
        }
    }
    let Some(turn) = finishing else {
        return Ok(Vec::new());
    };
    let mut tx = app.store.tx(lease.org_id).await?;
    let current: Option<Option<String>> = sqlx::query_scalar(
        "SELECT native_turn_id FROM runs WHERE org_id=$1 AND id=$2 AND state='running'",
    )
    .bind(lease.org_id)
    .bind(run)
    .fetch_optional(&mut *tx)
    .await?;
    // Another run's turn, or one not registered yet: its own pass hands over.
    if current.is_none() || turn.is_some_and(|turn| current.flatten().as_deref() != Some(&turn)) {
        return Ok(Vec::new());
    }
    let earlier = outputs::unhanded(&mut tx, lease.org_id, run).await?;
    tx.commit().await?;
    let texts = earlier
        .iter()
        .map(|item| item.text.as_str())
        .chain(texts.iter().map(String::as_str));
    let targets = outputs::gather(texts);
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    Ok(outputs::publish(app, lease, Some(sandbox), &targets).await)
}

/// A harness approval or question: answered automatically, or shown to the user.
#[allow(clippy::too_many_arguments)]
async fn request(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    sandbox: Uuid,
    request_id: &str,
    event: &HarnessEvent,
    kind: &str,
    summary: &str,
    automatic: Option<HarnessAnswer>,
    config: &crate::config::Config,
) -> anyhow::Result<()> {
    let params = serde_json::to_value(event)?;
    if let Some(answer) = automatic {
        sqlx::query(
            "INSERT INTO native_requests(org_id,sandbox_id,request_id,run_id,method,params,answer)
             VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING",
        )
        .bind(lease.org_id)
        .bind(sandbox)
        .bind(request_id)
        .bind(run)
        .bind(kind)
        .bind(&params)
        .bind(serde_json::to_value(answer)?)
        .execute(&mut **tx)
        .await?;
        return Ok(());
    }
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM native_requests WHERE org_id=$1 AND sandbox_id=$2 AND request_id=$3)",
    )
    .bind(lease.org_id)
    .bind(sandbox)
    .bind(request_id)
    .fetch_one(&mut **tx)
    .await?;
    if exists {
        return Ok(());
    }
    let options: &[&str] = if kind == "question" {
        &[]
    } else {
        &["allow", "deny"]
    };
    let action = runs::new_action(tx, lease, run, kind, summary, options, config).await?;
    sqlx::query(
        "INSERT INTO native_requests(org_id,sandbox_id,request_id,run_id,method,params,action_id)
         VALUES($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(lease.org_id)
    .bind(sandbox)
    .bind(request_id)
    .bind(run)
    .bind(kind)
    .bind(&params)
    .bind(action.id)
    .execute(&mut **tx)
    .await?;
    runs::request_action(tx, lease, run, action).await
}

/// The harness answer for a user's decision on a projected request.
pub fn answer(request: &Value, kind: &str, text: &str) -> anyhow::Result<HarnessAnswer> {
    if kind != "question" {
        return Ok(HarnessAnswer::Approval {
            allow: text == "allow",
        });
    }
    let HarnessEvent::QuestionRequested { questions, .. } =
        serde_json::from_value(request.clone())?
    else {
        anyhow::bail!(Code::InvalidAction);
    };
    let supplied: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let mut answers = std::collections::BTreeMap::new();
    for question in &questions {
        let answer = if questions.len() == 1 {
            text
        } else {
            // Several questions are answered with a JSON object keyed by question id.
            supplied[&question.id]
                .as_str()
                .context(Code::InvalidMessage)?
        };
        answers.insert(question.id.clone(), answer.to_owned());
    }
    Ok(HarnessAnswer::Question { answers })
}

fn timeline_kind(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Assistant => "assistant",
        ItemKind::Reasoning => "reasoning",
        ItemKind::Plan => "plan",
        ItemKind::FileChange => "file_change",
        ItemKind::Command | ItemKind::ToolCall | ItemKind::Other => "tool",
    }
}

fn detail(kind: ItemKind, content: &ItemContent) -> Option<ItemDetail> {
    let kind = match kind {
        ItemKind::Command | ItemKind::ToolCall => item_detail::Kind::Tool(ToolDetail {
            name: content.name.clone().unwrap_or_default(),
            arguments_json: content.arguments_json.clone().unwrap_or_default(),
            result_json: content.result_json.clone().unwrap_or_default(),
            output: content.output.clone().unwrap_or_default(),
            ..Default::default()
        }),
        ItemKind::FileChange => item_detail::Kind::FileChange(FileChangeDetail {
            changes: content
                .file_changes
                .iter()
                .map(|change| FileChange {
                    path: change.path.clone(),
                    change: change.change.clone(),
                    diff: change.diff.clone(),
                })
                .collect(),
        }),
        _ => return None,
    };
    Some(ItemDetail { kind: Some(kind) })
}

#[allow(clippy::too_many_arguments)]
/// A harness item as the projection reads it.
struct Native<'a> {
    phase: ItemPhase,
    id: &'a str,
    kind: ItemKind,
    content: &'a ItemContent,
    message_id: Option<&'a str>,
    /// The native item it belongs to.
    parent: Option<&'a str>,
}

async fn project_item(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    sandbox: Uuid,
    native: Native<'_>,
) -> anyhow::Result<()> {
    let Native {
        phase,
        id: native_id,
        kind,
        content,
        message_id,
        parent,
    } = native;
    let mapped: Option<Uuid> = sqlx::query_scalar(
        "SELECT item_id FROM native_items WHERE org_id=$1 AND sandbox_id=$2 AND native_id=$3",
    )
    .bind(lease.org_id)
    .bind(sandbox)
    .bind(native_id)
    .fetch_optional(&mut **tx)
    .await?;
    let mut item = match mapped {
        Some(id) => load_item(tx, lease.org_id, id).await?,
        None => {
            // The first assistant message takes over the run's placeholder item.
            let primary: Uuid =
                sqlx::query_scalar("SELECT assistant_item_id FROM runs WHERE org_id=$1 AND id=$2")
                    .bind(lease.org_id)
                    .bind(run)
                    .fetch_one(&mut **tx)
                    .await?;
            let used: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM native_items WHERE org_id=$1 AND item_id=$2)",
            )
            .bind(lease.org_id)
            .bind(primary)
            .fetch_one(&mut **tx)
            .await?;
            let mut item = if kind == ItemKind::Assistant && !used {
                load_item(tx, lease.org_id, primary).await?
            } else {
                Item {
                    id: Uuid::now_v7().to_string(),
                    kind: timeline_kind(kind).into(),
                    run_id: run.to_string(),
                    ..Default::default()
                }
            };
            item.revision += 1;
            item.state = "streaming".into();
            let item = upsert_item(tx, lease, item).await?;
            sqlx::query(
                "INSERT INTO native_items(org_id,sandbox_id,native_id,item_id) VALUES($1,$2,$3,$4)",
            )
            .bind(lease.org_id)
            .bind(sandbox)
            .bind(native_id)
            .bind(Uuid::parse_str(&item.id)?)
            .execute(&mut **tx)
            .await?;
            item
        }
    };
    item.revision += 1;
    item.state = match phase {
        ItemPhase::Completed => "completed",
        ItemPhase::Started => "streaming",
    }
    .into();
    item.text = content.text.clone();
    item.detail = detail(kind, content);
    // An item keeps the parent it started with; a completion may not know it.
    if let Some(parent) = parent
        && item.parent_item_id.is_empty()
    {
        let parent: Option<Uuid> = sqlx::query_scalar(
            "SELECT item_id FROM native_items WHERE org_id=$1 AND sandbox_id=$2 AND native_id=$3",
        )
        .bind(lease.org_id)
        .bind(sandbox)
        .bind(parent)
        .fetch_optional(&mut **tx)
        .await?;
        item.parent_item_id = parent.map(|p| p.to_string()).unwrap_or_default();
    }
    if let Some(message) = message_id {
        sqlx::query(
            "UPDATE native_items SET native_message_id=$4 WHERE org_id=$1 AND sandbox_id=$2 AND native_id=$3",
        )
        .bind(lease.org_id)
        .bind(sandbox)
        .bind(native_id)
        .bind(message)
        .execute(&mut **tx)
        .await?;
    }
    upsert_item(tx, lease, item).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryzhub_execution::harness::Question;
    use serde_json::json;

    #[test]
    fn answers_follow_the_request_shape() {
        let request = serde_json::to_value(HarnessEvent::QuestionRequested {
            request_id: "1".into(),
            turn_id: None,
            questions: vec![
                Question {
                    id: "a".into(),
                    question: "A?".into(),
                    options: json!(null),
                },
                Question {
                    id: "b".into(),
                    question: "B?".into(),
                    options: json!(null),
                },
            ],
        })
        .unwrap();
        let HarnessAnswer::Question { answers } =
            answer(&request, "question", r#"{"a":"x","b":"y"}"#).unwrap()
        else {
            unreachable!()
        };
        assert_eq!(answers["b"], "y");
        assert!(answer(&request, "question", "plain").is_err());
        assert_eq!(
            answer(&json!({}), "approval", "allow").unwrap(),
            HarnessAnswer::Approval { allow: true }
        );
    }
}
