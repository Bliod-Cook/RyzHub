//! Durable command intake. A command is queued once per `(user, cid)`; retries of the same
//! payload return the first result, a different payload under the same cid is rejected.
use anyhow::{Context, ensure};
use prost::Message;
use ryzhub_domain::OrgContext;
use ryzhub_proto::{CommandEnvelope, CommandRejected, Frame, frame::Body};
use sqlx::Acquire;
use uuid::Uuid;

use super::{Store, Tx, context_in, conversation_in};
use crate::error::{Code, code_of, is_fenced};

/// What a command's cid stands for.
pub enum Cid {
    /// Taken now, by the command with this id.
    Taken(Uuid),
    /// Taken before by this same command: its stored result, None while it waits.
    Retry(Option<Box<Frame>>),
}

/// The command first sent under a cid.
#[derive(sqlx::FromRow)]
struct First {
    conversation_id: Option<Uuid>,
    payload: Vec<u8>,
    result: Option<Vec<u8>>,
}

/// The result of the command first sent under `cid` when this one retries it (None while it
/// waits); None when nothing was sent under `cid`. Any other command under it is refused. A
/// command on a user channel names no conversation.
pub async fn retry(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
    cid: Uuid,
    conv: Option<Uuid>,
    payload: &[u8],
) -> anyhow::Result<Option<Option<Box<Frame>>>> {
    let first: Option<First> = sqlx::query_as(
        "SELECT conversation_id,payload,result FROM commands WHERE org_id=$1 AND user_id=$2 AND cid=$3",
    )
    .bind(ctx.org_id)
    .bind(ctx.user_id)
    .bind(cid)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(first) = first else {
        return Ok(None);
    };
    ensure!(
        first.conversation_id == conv && first.payload == payload,
        Code::CidReused
    );
    let result = first.result.as_deref().map(Frame::decode).transpose()?;
    Ok(Some(result.map(Box::new)))
}

/// Take `cid` for the command `payload`, unless a command already has it.
pub async fn take_cid(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
    cid: Uuid,
    conv: Option<Uuid>,
    device: &str,
    payload: &[u8],
) -> anyhow::Result<Cid> {
    let taken: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO commands(id,user_id,cid,org_id,conversation_id,device_id,payload)
         VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(user_id,cid) DO NOTHING RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(ctx.user_id)
    .bind(cid)
    .bind(ctx.org_id)
    .bind(conv)
    .bind(device)
    .bind(payload)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(match taken {
        Some(id) => Cid::Taken(id),
        // Under the same cid in another organization: not visible here.
        None => Cid::Retry(
            retry(tx, ctx, cid, conv, payload)
                .await?
                .context(Code::CidReused)?,
        ),
    })
}

/// Run taken command `id` in a savepoint and store what became of it as its result: an Ack,
/// or a final rejection. Database failures and lost leases are returned instead; the command
/// stays to be retried.
pub async fn finish_command(
    tx: &mut Tx<'_>,
    org: Uuid,
    id: Uuid,
    channel: &str,
    cid: &str,
    execute: impl AsyncFnOnce(&mut Tx<'_>) -> anyhow::Result<Body>,
) -> anyhow::Result<Frame> {
    let mut savepoint = (&mut **tx).begin().await?;
    let body = match execute(&mut savepoint).await {
        Ok(body) => {
            savepoint.commit().await?;
            body
        }
        Err(error) => {
            savepoint.rollback().await?;
            if error.downcast_ref::<sqlx::Error>().is_some() || is_fenced(&error) {
                return Err(error);
            }
            let code = code_of(&error).unwrap_or_else(|| {
                tracing::error!(error = format!("{error:#}"), "command failed");
                Code::InternalError
            });
            Body::CommandRejected(CommandRejected {
                cid: cid.into(),
                code: code.to_string(),
                retryable: false,
            })
        }
    };
    let response = ryzhub_proto::frame(channel, body);
    sqlx::query("UPDATE commands SET status='done',result=$3 WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(id)
        .bind(response.encode_to_vec())
        .execute(&mut **tx)
        .await?;
    Ok(response)
}

impl Store {
    /// Queue the command. Returns the stored result when this cid was already processed.
    pub async fn accept(
        &self,
        org: Uuid,
        user: Uuid,
        conv: Uuid,
        device: &str,
        cmd: &CommandEnvelope,
    ) -> anyhow::Result<Option<Frame>> {
        let cid = Uuid::parse_str(&cmd.cid).context(Code::InvalidCid)?;
        ensure!(cmd.command.is_some(), Code::InvalidCommand);
        let payload = cmd.encode_to_vec();
        let mut tx = self.tx(org).await?;
        let ctx = context_in(&mut tx, user, org).await?;
        conversation_in(&mut tx, &ctx, conv).await?;
        let result = match take_cid(&mut tx, &ctx, cid, Some(conv), device, &payload).await? {
            Cid::Taken(_) => None,
            Cid::Retry(result) => result.map(|frame| *frame),
        };
        tx.commit().await?;
        Ok(result)
    }

    /// The stored Ack/Rejection, once the Owner has processed the command.
    pub async fn command_result(
        &self,
        org: Uuid,
        user: Uuid,
        cid: Uuid,
    ) -> anyhow::Result<Option<Frame>> {
        let mut tx = self.tx(org).await?;
        let result = sqlx::query_scalar!(
            "SELECT result FROM commands WHERE org_id=$1 AND user_id=$2 AND cid=$3",
            org,
            user,
            cid
        )
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        result
            .map(|bytes| Frame::decode(bytes.as_slice()).map_err(Into::into))
            .transpose()
    }
}
