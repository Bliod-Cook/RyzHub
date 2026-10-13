//! Commands on a user channel, which the Gateway takes itself. CreateRun creates its
//! conversation and queues itself for that conversation's Owner, which runs it as the first
//! message; UndoMemoryUpdate is answered here. Like any command, each is taken once per
//! `(user, cid)`: a retry gets the first result.
use anyhow::{Context, bail, ensure};
use prost::Message;
use ryzhub_domain::OrgContext;
use ryzhub_proto::{command_envelope::Command, frame::Body, *};
use serde_json::{Value, json};
use sqlx::Acquire;
use uuid::Uuid;

use crate::{
    app::App,
    error::{Code, code_of},
    runs,
    store::{Tx, context_in, create_in},
};

/// What became of a command.
pub enum Intake {
    /// Answered: its result.
    Done(Box<Frame>),
    /// Queued for the Owner of this conversation.
    Queued(Uuid),
}

/// The command first sent under the cid, if any: its conversation, payload and result.
#[derive(sqlx::FromRow)]
struct First {
    conversation_id: Option<Uuid>,
    payload: Vec<u8>,
    result: Option<Vec<u8>>,
}

async fn first(tx: &mut Tx<'_>, ctx: &OrgContext, cid: Uuid) -> anyhow::Result<Option<First>> {
    Ok(sqlx::query_as(
        "SELECT conversation_id,payload,result FROM commands WHERE org_id=$1 AND user_id=$2 AND cid=$3",
    )
    .bind(ctx.org_id)
    .bind(ctx.user_id)
    .bind(cid)
    .fetch_optional(&mut **tx)
    .await?)
}

pub async fn accept(
    app: &App,
    ctx: &OrgContext,
    channel: &str,
    device: &str,
    envelope: &CommandEnvelope,
) -> anyhow::Result<Intake> {
    let cid = Uuid::parse_str(&envelope.cid).context(Code::InvalidCid)?;
    match envelope.command.as_ref().context(Code::InvalidCommand)? {
        Command::CreateRun(create) => create_run(app, ctx, device, cid, envelope, create).await,
        Command::UndoMemoryUpdate(undo) => {
            undo_memory(app, ctx, channel, device, cid, envelope, undo).await
        }
        _ => bail!(Code::InvalidCommand),
    }
}

/// Creates the conversation and queues the command under it, in one transaction.
async fn create_run(
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
    let mut tx = app.store.tx(ctx.org_id).await?;
    let ctx = context_in(&mut tx, ctx.user_id, ctx.org_id).await?;
    if let Some(intake) = retry(&mut tx, &ctx, cid, conv, &payload).await? {
        tx.commit().await?;
        return Ok(intake);
    }
    let created = create_in(
        &mut tx,
        &ctx,
        conv,
        &title,
        &create.mode,
        settings,
        project,
        Some(&app.config),
    )
    .await?;
    if created.is_none() {
        // The same command sent twice at once: the other took the id first.
        if let Some(intake) = retry(&mut tx, &ctx, cid, conv, &payload).await? {
            tx.commit().await?;
            return Ok(intake);
        }
        bail!(Code::ConversationIdTaken);
    }
    sqlx::query(
        "INSERT INTO commands(id,user_id,cid,org_id,conversation_id,device_id,payload)
         VALUES($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(Uuid::now_v7())
    .bind(ctx.user_id)
    .bind(cid)
    .bind(ctx.org_id)
    .bind(conv)
    .bind(device)
    .bind(&payload)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Intake::Queued(conv))
}

/// A retry of a command already taken: its result, or its conversation's Owner to wait for.
async fn retry(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
    cid: Uuid,
    conv: Uuid,
    payload: &[u8],
) -> anyhow::Result<Option<Intake>> {
    let Some(first) = first(tx, ctx, cid).await? else {
        return Ok(None);
    };
    ensure!(
        first.conversation_id == Some(conv) && first.payload == payload,
        Code::CidReused
    );
    Ok(Some(match first.result {
        Some(result) => Intake::Done(Box::new(Frame::decode(result.as_slice())?)),
        None => Intake::Queued(conv),
    }))
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
    app: &App,
    ctx: &OrgContext,
    channel: &str,
    device: &str,
    cid: Uuid,
    envelope: &CommandEnvelope,
    undo: &UndoMemoryUpdate,
) -> anyhow::Result<Intake> {
    let event = Uuid::parse_str(&undo.event_id).context(Code::InvalidArguments)?;
    let payload = envelope.encode_to_vec();
    let mut tx = app.store.tx(ctx.org_id).await?;
    let ctx = context_in(&mut tx, ctx.user_id, ctx.org_id).await?;
    // Taking the cid first makes a concurrent retry wait for this answer.
    let taken: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO commands(id,user_id,cid,org_id,conversation_id,device_id,payload)
         VALUES($1,$2,$3,$4,NULL,$5,$6) ON CONFLICT(user_id,cid) DO NOTHING RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(ctx.user_id)
    .bind(cid)
    .bind(ctx.org_id)
    .bind(device)
    .bind(&payload)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = taken else {
        let first = first(&mut tx, &ctx, cid).await?.context(Code::CidReused)?;
        ensure!(
            first.conversation_id.is_none() && first.payload == payload,
            Code::CidReused
        );
        let result = first.result.context(Code::TemporarilyUnavailable)?;
        return Ok(Intake::Done(Box::new(Frame::decode(result.as_slice())?)));
    };
    let mut savepoint = tx.begin().await?;
    let body = match crate::memory::undo(&mut savepoint, ctx.org_id, ctx.user_id, event).await {
        Ok(_) => {
            savepoint.commit().await?;
            Body::CommandAck(CommandAck {
                cid: envelope.cid.clone(),
                accepted_seq: 0,
                result: "undone".into(),
            })
        }
        Err(error) => {
            savepoint.rollback().await?;
            if error.downcast_ref::<sqlx::Error>().is_some() {
                return Err(error);
            }
            // Already undone, or changed since: undoing again cannot succeed.
            let code = match code_of(&error) {
                Some(Code::ConversationBusy) => Code::MemoryUndoUnavailable,
                Some(code) => code,
                None => return Err(error),
            };
            Body::CommandRejected(CommandRejected {
                cid: envelope.cid.clone(),
                code: code.to_string(),
                retryable: false,
            })
        }
    };
    let response = frame(channel, body);
    sqlx::query("UPDATE commands SET status='done',result=$3 WHERE org_id=$1 AND id=$2")
        .bind(ctx.org_id)
        .bind(id)
        .bind(response.encode_to_vec())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
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
