//! Commands on a user channel, which the Gateway takes itself. CreateRun creates its
//! conversation and queues itself for that conversation's Owner, which runs it as the first
//! message; UndoMemoryUpdate is answered here. Like any command, each is taken once per
//! `(user, cid)`: a retry gets the first result.
use anyhow::{Context, bail, ensure};
use prost::Message;
use ryzhub_domain::OrgContext;
use ryzhub_proto::{command_envelope::Command, frame::Body, *};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::App,
    error::Code,
    runs,
    store::{Cid, Tx, context_in, create_in, finish_command, retry, take_cid},
};

/// What became of a command.
pub enum Intake {
    /// Answered: its result.
    Done(Box<Frame>),
    /// Queued for the Owner of this conversation.
    Queued(Uuid),
}

/// Take a command on the user channel of `user` in `org`. Membership is checked here, in the
/// transaction that takes the command.
pub async fn accept(
    app: &App,
    (org, user): (Uuid, Uuid),
    channel: &str,
    device: &str,
    envelope: &CommandEnvelope,
) -> anyhow::Result<Intake> {
    let cid = Uuid::parse_str(&envelope.cid).context(Code::InvalidCid)?;
    let mut tx = app.store.tx(org).await?;
    let ctx = context_in(&mut tx, user, org).await?;
    let intake = match envelope.command.as_ref().context(Code::InvalidCommand)? {
        Command::CreateRun(create) => {
            create_run(&mut tx, app, &ctx, device, cid, envelope, create).await?
        }
        Command::UndoMemoryUpdate(undo) => {
            undo_memory(&mut tx, &ctx, channel, device, cid, envelope, undo).await?
        }
        _ => bail!(Code::InvalidCommand),
    };
    tx.commit().await?;
    Ok(intake)
}

/// What became of a command, from its stored result.
fn intake(conv: Uuid, result: Option<Box<Frame>>) -> Intake {
    match result {
        Some(result) => Intake::Done(result),
        None => Intake::Queued(conv),
    }
}

/// Creates the conversation and queues the command under it, in one transaction.
async fn create_run(
    tx: &mut Tx<'_>,
    app: &App,
    ctx: &OrgContext,
    device: &str,
    cid: Uuid,
    envelope: &CommandEnvelope,
    create: &CreateRun,
) -> anyhow::Result<Intake> {
    let conv = Uuid::parse_str(&create.conversation_id)
        .ok()
        .filter(|id| !id.is_nil())
        .context(Code::InvalidConversation)?;
    let message = create
        .first_message
        .as_ref()
        .context(Code::InvalidMessage)?;
    // Checked here so the Owner rarely refuses what has already made a conversation.
    runs::valid_input(&runs::Input {
        text: &message.text,
        files: &message.attachment_ids,
        user: ctx.user_id,
    })?;
    let mut settings: Value = if create.run_config_json.is_empty() {
        json!({})
    } else {
        serde_json::from_str(&create.run_config_json).map_err(|_| Code::InvalidConfig)?
    };
    ensure!(settings.is_object(), Code::InvalidConfig);
    if create.temporary {
        settings["temporary"] = json!(true);
    }
    let project = if create.project_id.is_empty() {
        None
    } else {
        Some(Uuid::parse_str(&create.project_id).context(Code::InvalidConversation)?)
    };
    let title = if create.title.trim().is_empty() {
        title_of(&message.text)
    } else {
        create.title.clone()
    };
    let payload = envelope.encode_to_vec();
    // Answered before anything is validated again: a retry gets the first result.
    if let Some(result) = retry(tx, ctx, cid, Some(conv), &payload).await? {
        return Ok(intake(conv, result));
    }
    let created = create_in(
        tx,
        ctx,
        conv,
        &title,
        &create.mode,
        settings,
        project,
        &app.config(),
    )
    .await?;
    if created.is_none() {
        // The same command sent twice at once: the other took the id first.
        let result = retry(tx, ctx, cid, Some(conv), &payload).await?;
        return Ok(intake(conv, result.context(Code::ConversationIdTaken)?));
    }
    let Cid::Taken(_) = take_cid(tx, ctx, cid, Some(conv), device, &payload).await? else {
        bail!(Code::CidReused);
    };
    Ok(Intake::Queued(conv))
}

/// A conversation's title from its first message: its first line that is not a fence.
fn title_of(text: &str) -> String {
    let line = text
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|l| !l.is_empty() && !l.starts_with("```"));
    match line {
        Some(line) if line.chars().count() > 40 => {
            format!("{}…", line.chars().take(40).collect::<String>())
        }
        Some(line) => line,
        None => "新对话".into(),
    }
}

async fn undo_memory(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
    channel: &str,
    device: &str,
    cid: Uuid,
    envelope: &CommandEnvelope,
    undo: &UndoMemoryUpdate,
) -> anyhow::Result<Intake> {
    let event = Uuid::parse_str(&undo.event_id).context(Code::InvalidArguments)?;
    let payload = envelope.encode_to_vec();
    // Taking the cid first makes a concurrent retry wait for this answer.
    let id = match take_cid(tx, ctx, cid, None, device, &payload).await? {
        Cid::Taken(id) => id,
        Cid::Retry(result) => {
            return Ok(Intake::Done(result.context(Code::TemporarilyUnavailable)?));
        }
    };
    let (org, user) = (ctx.org_id, ctx.user_id);
    let response = finish_command(
        tx,
        org,
        id,
        channel,
        &envelope.cid,
        async |tx: &mut Tx<'_>| {
            crate::memory::undo(tx, org, user, event).await?;
            Ok(Body::CommandAck(CommandAck {
                cid: envelope.cid.clone(),
                accepted_seq: 0,
                result: "undone".into(),
            }))
        },
    )
    .await?;
    Ok(Intake::Done(Box::new(response)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(super::title_of("```rust\nfn\n```"), "fn");
        assert_eq!(super::title_of("  hello   world \nmore"), "hello world");
        assert_eq!(super::title_of(""), "新对话");
        assert_eq!(super::title_of(&"字".repeat(50)).chars().count(), 41);
    }
}
