//! Ordered execution of queued commands by the conversation's Owner.
use anyhow::{Context, ensure};
use prost::Message;
use ryzhub_proto::{command_envelope::Command, frame::Body, *};
use sqlx::Acquire;
use uuid::Uuid;

use super::Engine;
use crate::{
    config::Config,
    conversation_commands,
    error::{Code, code_of, is_fenced},
    runs::{self, Resolution, RunKind},
    store::{Lease, Store, Tx, context_in, conversation_in, head_seq, lock_lease},
};

/// A processed command and the frame stored as its result.
pub struct Processed {
    pub user: Uuid,
    pub cid: Uuid,
    pub response: Frame,
}

/// Execute the oldest queued command. The command's effects, its stored result and the
/// events it produced commit in one transaction under the lease.
pub async fn process_next(
    store: &Store,
    lease: &Lease,
    config: &Config,
) -> anyhow::Result<Option<Processed>> {
    let mut tx = store.tx(lease.org_id).await?;
    // Lock the fencing row before touching commands or projections.
    lock_lease(&mut tx, lease).await?;
    let row: Option<(Uuid, Uuid, Uuid, String, Vec<u8>)> = sqlx::query_as(
        "SELECT id,user_id,cid,device_id,payload FROM commands
         WHERE org_id=$1 AND conversation_id=$2 AND status='queued' ORDER BY created_at,id LIMIT 1 FOR UPDATE",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((command_id, user, cid, device, payload)) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let command = CommandEnvelope::decode(payload.as_slice())?;
    let mut savepoint = tx.begin().await?;
    let body = match execute(&mut savepoint, lease, user, &device, &command, config).await {
        Ok(body) => {
            savepoint.commit().await?;
            body
        }
        Err(error) => {
            savepoint.rollback().await?;
            // Database failures and lost leases are retried by the next Owner; the command
            // stays queued. Everything else is a final answer for this cid.
            if error.downcast_ref::<sqlx::Error>().is_some() || is_fenced(&error) {
                return Err(error);
            }
            let code = code_of(&error).unwrap_or_else(|| {
                tracing::error!(error = format!("{error:#}"), "command failed");
                Code::InternalError
            });
            Body::CommandRejected(CommandRejected {
                cid: command.cid.clone(),
                code: code.to_string(),
                retryable: false,
            })
        }
    };
    let accepted = matches!(body, Body::CommandAck(_));
    let response = frame(lease.channel(), body);
    sqlx::query("UPDATE commands SET status='done',result=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(command_id)
        .bind(response.encode_to_vec())
        .execute(&mut *tx)
        .await?;
    crate::store::audit_as(
        &mut tx,
        lease.org_id,
        user,
        "user",
        "command",
        &lease.channel(),
        if accepted { "accepted" } else { "rejected" },
    )
    .await?;
    tx.commit().await?;
    Ok(Some(Processed {
        user,
        cid,
        response,
    }))
}

/// Test and tooling entry point: process one command with the given configuration.
pub async fn process_with_config(
    store: &Store,
    lease: &Lease,
    config: &Config,
) -> anyhow::Result<bool> {
    Ok(process_next(store, lease, config).await?.is_some())
}

/// Process one command with the default (simulated) configuration.
pub async fn process_command(store: &Store, lease: &Lease) -> anyhow::Result<bool> {
    process_with_config(store, lease, &Config::default()).await
}

async fn execute(
    tx: &mut Tx<'_>,
    lease: &Lease,
    user: Uuid,
    device: &str,
    envelope: &CommandEnvelope,
    config: &Config,
) -> anyhow::Result<Body> {
    let ctx = context_in(tx, user, lease.org_id).await?;
    let conversation = conversation_in(tx, &ctx, lease.conversation_id).await?;
    ensure!(conversation.state == "active", Code::ConversationArchived);
    let engine = Engine::of(config, &conversation.mode);
    let cid = envelope.cid.as_str();
    let command = envelope.command.as_ref().context(Code::InvalidCommand)?;
    // The simulator handles messages, approvals and cancellation only.
    ensure!(
        engine != Engine::Mock
            || matches!(
                command,
                Command::SendMessage(_)
                    | Command::CreateRun(_)
                    | Command::DecideApproval(_)
                    | Command::Cancel(_)
                    | Command::MarkRead(_)
            ),
        Code::UnsupportedCommand
    );
    match command {
        Command::SendMessage(message) => {
            // Settings sent with the message apply before its run, so a separately sent
            // UpdateRunConfig cannot land after it.
            if engine != Engine::Mock && !message.config_json.is_empty() {
                conversation_commands::configure(tx, lease, &message.config_json, config).await?;
            }
            let input = runs::Input {
                text: &message.text,
                files: &message.attachment_ids,
                user,
            };
            runs::create(tx, lease, envelope, input, RunKind::Main, engine, config).await?;
        }
        // Created with its conversation: the run configuration was set then.
        Command::CreateRun(create) => {
            let message = create
                .first_message
                .as_ref()
                .context(Code::InvalidMessage)?;
            let input = runs::Input {
                text: &message.text,
                files: &message.attachment_ids,
                user,
            };
            runs::create(tx, lease, envelope, input, RunKind::Main, engine, config).await?;
        }
        Command::UndoMemoryUpdate(_) => anyhow::bail!(Code::InvalidCommand),
        Command::InvokeMenuCommand(invoke) => {
            let installation =
                Uuid::parse_str(&invoke.installation_id).map_err(|_| Code::InvalidArguments)?;
            let params: serde_json::Map<String, serde_json::Value> =
                if invoke.params_json.is_empty() {
                    serde_json::Map::new()
                } else {
                    serde_json::from_str(&invoke.params_json).map_err(|_| Code::InvalidArguments)?
                };
            let text = crate::plugins::command_prompt(
                tx,
                lease.org_id,
                lease.conversation_id,
                installation,
                &invoke.command,
                &params,
            )
            .await?;
            let input = runs::Input {
                text: &text,
                files: &invoke.attachment_ids,
                user,
            };
            runs::create(tx, lease, envelope, input, RunKind::Main, engine, config).await?;
        }
        Command::SideQuery(query) => {
            runs::create(
                tx,
                lease,
                envelope,
                runs::Input::text(&query.text),
                RunKind::SideQuery,
                engine,
                config,
            )
            .await?;
        }
        Command::Steer(steer) => {
            let input = runs::Input {
                text: &steer.text,
                files: &steer.attachment_ids,
                user,
            };
            runs::steer(tx, lease, input, cid).await?
        }
        Command::DecideApproval(decision) => {
            ensure!(
                matches!(decision.decision.as_str(), "allow" | "deny"),
                Code::InvalidDecision
            );
            let run = runs::resolve(
                tx,
                lease,
                Resolution {
                    user,
                    device,
                    action_id: &decision.action_id,
                    run_epoch: decision.run_epoch,
                    kind: "approval",
                    answer: &decision.decision,
                    cid,
                },
            )
            .await?;
            // The simulator has no tool to report a denial to; denying ends the run.
            if engine == Engine::Mock && decision.decision == "deny" {
                runs::finish(tx, lease, run, "cancelled", None).await?;
            }
        }
        Command::AnswerQuestion(answer) => {
            runs::valid_text(&answer.answer)?;
            runs::resolve(
                tx,
                lease,
                Resolution {
                    user,
                    device,
                    action_id: &answer.action_id,
                    run_epoch: answer.run_epoch,
                    kind: "question",
                    answer: &answer.answer,
                    cid,
                },
            )
            .await?;
        }
        Command::Cancel(_) => runs::cancel_all(tx, lease).await?,
        Command::MarkRead(read) => runs::mark_read(tx, lease, &ctx, read.read_seq, cid).await?,
        Command::UpdateRunConfig(update) => {
            conversation_commands::configure(tx, lease, &update.config_json, config).await?
        }
        Command::Compact(_) => {
            conversation_commands::ensure_compactable(tx, lease, config).await?;
            runs::create(
                tx,
                lease,
                envelope,
                runs::Input::text("/compact"),
                RunKind::Compact,
                engine,
                config,
            )
            .await?;
        }
        Command::Fork(fork) => {
            conversation_commands::enqueue_fork(tx, lease, &fork.item_id, cid).await?
        }
    }
    Ok(Body::CommandAck(CommandAck {
        cid: cid.into(),
        accepted_seq: head_seq(tx, lease.org_id, &lease.channel()).await?,
        result: "accepted".into(),
    }))
}
