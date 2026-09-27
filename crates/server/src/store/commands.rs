//! Durable command intake. A command is queued once per `(user, cid)`; retries of the same
//! payload return the first result, a different payload under the same cid is rejected.
use anyhow::{Context, ensure};
use prost::Message;
use ryzhub_domain::OrgContext;
use ryzhub_proto::{CommandEnvelope, Frame};
use uuid::Uuid;

use super::{Store, context_in, conversation_in};
use crate::error::Code;

impl Store {
    /// Queue the command. Returns the stored result when this cid was already processed.
    pub async fn accept(
        &self,
        ctx: &OrgContext,
        conv: Uuid,
        device: &str,
        cmd: &CommandEnvelope,
    ) -> anyhow::Result<Option<Frame>> {
        let cid = Uuid::parse_str(&cmd.cid).context(Code::InvalidCid)?;
        ensure!(cmd.command.is_some(), Code::InvalidCommand);
        let payload = cmd.encode_to_vec();
        let mut tx = self.tx(ctx.org_id).await?;
        let ctx = context_in(&mut tx, ctx.user_id, ctx.org_id).await?;
        conversation_in(&mut tx, &ctx, conv).await?;
        sqlx::query!(
            "INSERT INTO commands(id,user_id,cid,org_id,conversation_id,device_id,payload)
             VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(user_id,cid) DO NOTHING",
            Uuid::now_v7(),
            ctx.user_id,
            cid,
            ctx.org_id,
            conv,
            device,
            &payload
        )
        .execute(&mut *tx)
        .await?;
        // The first command with this cid, which a retry must match exactly.
        let first = sqlx::query!(
            "SELECT conversation_id,payload,result FROM commands WHERE org_id=$1 AND user_id=$2 AND cid=$3",
            ctx.org_id,
            ctx.user_id,
            cid
        )
        .fetch_optional(&mut *tx)
        .await?
        .context(Code::CidReused)?;
        ensure!(
            first.conversation_id == conv && first.payload == payload,
            Code::CidReused
        );
        tx.commit().await?;
        first
            .result
            .map(|bytes| Frame::decode(bytes.as_slice()).map_err(Into::into))
            .transpose()
    }

    /// The stored Ack/Rejection, once the Owner has processed the command.
    pub async fn command_result(
        &self,
        ctx: &OrgContext,
        cid: Uuid,
    ) -> anyhow::Result<Option<Frame>> {
        let mut tx = self.tx(ctx.org_id).await?;
        let result = sqlx::query_scalar!(
            "SELECT result FROM commands WHERE org_id=$1 AND user_id=$2 AND cid=$3",
            ctx.org_id,
            ctx.user_id,
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
