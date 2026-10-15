//! Channel sequence allocation, the durable event log, leases and replay.
//!
//! Every durable change is written by [`append`] in the caller's transaction: the channel row
//! allocates the next `seq` under the writer's lease, the projection is upserted and the event
//! row doubles as the publication outbox.
use anyhow::{Context, ensure};
use prost::Message;
use ryzhub_domain::{Action, OrgContext, Principal, Resource, authorize};
use ryzhub_proto::{event_envelope::Event, frame::Body, *};
use sqlx::{Row, types::Json};
use uuid::Uuid;

use super::{ActionRow, Conversation, ItemRow, Lease, Store, Tx};
use crate::error::Code;

/// Display text beyond this is cut; NATS messages and client frames stay far below 1 MiB.
pub const MAX_ITEM_TEXT: usize = 256 * 1024;
/// Each structured detail field (arguments, result, output, diff) is bounded separately.
pub const MAX_DETAIL_FIELD: usize = 64 * 1024;
/// Items in a snapshot; older items are paged over REST.
pub const SNAPSHOT_ITEMS: i64 = 100;

/// Cut `text` at a character boundary. Returns whether anything was removed.
pub fn truncate_utf8(text: &mut String, limit: usize) -> bool {
    if text.len() <= limit {
        return false;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

/// A conversation as its creator's user channel carries it: title, state and pin.
pub fn conversation_item(conversation: &Conversation, revision: u64) -> Item {
    Item {
        id: conversation.id.to_string(),
        revision,
        kind: "conversation".into(),
        state: conversation.state.clone(),
        text: conversation.title.clone(),
        detail: Some(ItemDetail {
            kind: Some(item_detail::Kind::Conversation(ConversationDetail {
                pinned_at_ms: conversation.pinned_at_ms.unwrap_or(0) as u64,
                activity_seq: conversation.activity_seq as u64,
            })),
        }),
        ..Default::default()
    }
}

/// Publishes the conversation's item to its creator's user channel as it now is.
pub async fn publish_conversation(tx: &mut Tx<'_>, org: Uuid, conv: Uuid) -> anyhow::Result<()> {
    let conversation: Conversation = sqlx::query_as(concat!(
        "SELECT ",
        conversation_columns!(),
        " FROM conversations WHERE org_id=$1 AND id=$2"
    ))
    .bind(org)
    .bind(conv)
    .fetch_one(&mut **tx)
    .await?;
    let channel = format!("user:{org}:{}", conversation.creator_id);
    // A user-channel sequence is also a monotonic metadata revision. Lock before allocating
    // it so concurrent conversations cannot reuse it.
    let revision: i64 =
        sqlx::query_scalar("SELECT last_seq+1 FROM channels WHERE org_id=$1 AND id=$2 FOR UPDATE")
            .bind(org)
            .bind(&channel)
            .fetch_one(&mut **tx)
            .await?;
    append(
        tx,
        org,
        &channel,
        None,
        "",
        Event::ItemUpserted(conversation_item(&conversation, revision as u64)),
    )
    .await?;
    Ok(())
}

fn bound_item(item: &mut Item) {
    item.truncated |= truncate_utf8(&mut item.text, MAX_ITEM_TEXT);
    let Some(detail) = item.detail.as_mut() else {
        return;
    };
    use item_detail::Kind;
    let mut cut = false;
    match detail.kind.as_mut() {
        Some(Kind::Tool(tool)) => {
            for field in [
                &mut tool.name,
                &mut tool.arguments_json,
                &mut tool.result_json,
                &mut tool.output,
            ] {
                cut |= truncate_utf8(field, MAX_DETAIL_FIELD);
            }
        }
        Some(Kind::FileChange(files)) => {
            files.changes.truncate(256);
            for change in &mut files.changes {
                cut |= truncate_utf8(&mut change.diff, MAX_DETAIL_FIELD);
            }
        }
        Some(Kind::Answer(answer)) => {
            answer.citations.truncate(64);
            for citation in &mut answer.citations {
                for field in [
                    &mut citation.url,
                    &mut citation.title,
                    &mut citation.cited_text,
                ] {
                    cut |= truncate_utf8(field, 4096);
                }
            }
            answer
                .files
                .truncate(crate::attachments::outputs::MAX_PER_ANSWER);
            for shared in &mut answer.files {
                cut |= truncate_utf8(&mut shared.link, 1024);
                cut |= truncate_utf8(&mut shared.error, 64);
                if let Some(file) = &mut shared.file {
                    for field in [
                        &mut file.id,
                        &mut file.path,
                        &mut file.name,
                        &mut file.media_type,
                        &mut file.kind,
                    ] {
                        cut |= truncate_utf8(field, 1024);
                    }
                }
            }
        }
        Some(Kind::Attachments(attachments)) => {
            attachments
                .files
                .truncate(crate::attachments::MAX_PER_MESSAGE);
            for file in &mut attachments.files {
                for field in [
                    &mut file.id,
                    &mut file.path,
                    &mut file.name,
                    &mut file.media_type,
                    &mut file.kind,
                ] {
                    cut |= truncate_utf8(field, 1024);
                }
            }
        }
        Some(Kind::Conversation(_)) | None => {}
    }
    item.truncated |= cut;
}

/// Allocate the next seq under `lease` (conversation channels) or the channel row lock
/// (user/org channels), upsert the item projection and record the event.
pub async fn append(
    tx: &mut Tx<'_>,
    org: Uuid,
    channel: &str,
    lease: Option<&Lease>,
    cid: &str,
    mut event: Event,
) -> anyhow::Result<EventEnvelope> {
    let seq: Option<i64> = sqlx::query_scalar!(
        "UPDATE channels SET last_seq=last_seq+1 WHERE org_id=$1 AND id=$2 AND (($3::bigint IS NULL AND writer_epoch IS NULL) OR (writer_epoch=$3 AND owner_id=$4 AND lease_until>clock_timestamp())) RETURNING last_seq",
        org,
        channel,
        lease.map(|l| l.epoch),
        lease.map(|l| l.owner_id)
    )
    .fetch_optional(&mut **tx)
    .await?;
    let seq = seq.context(Code::Fenced)?;
    if let Event::ItemUpserted(item) = &mut event {
        bound_item(item);
        item.last_seq = seq as u64;
        if item.first_seq == 0 {
            item.first_seq = seq as u64;
        }
        if let Some(lease) = lease {
            sqlx::query(
                "INSERT INTO items(id,org_id,conversation_id,run_id,revision,kind,state,text,first_seq,last_seq,truncated,detail,parent_item_id)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
                 ON CONFLICT(id) DO UPDATE SET revision=EXCLUDED.revision,state=EXCLUDED.state,text=EXCLUDED.text,
                     last_seq=EXCLUDED.last_seq,truncated=EXCLUDED.truncated,detail=EXCLUDED.detail,
                     parent_item_id=EXCLUDED.parent_item_id
                 WHERE items.org_id=EXCLUDED.org_id AND items.revision<EXCLUDED.revision",
            )
            .bind(Uuid::parse_str(&item.id)?)
            .bind(org)
            .bind(lease.conversation_id)
            .bind(Uuid::parse_str(&item.run_id).ok())
            .bind(item.revision as i64)
            .bind(&item.kind)
            .bind(&item.state)
            .bind(&item.text)
            .bind(item.first_seq as i64)
            .bind(seq)
            .bind(item.truncated)
            .bind(item.detail.as_ref().map(Json))
            .bind(Uuid::parse_str(&item.parent_item_id).ok())
            .execute(&mut **tx)
            .await?;
        }
    }
    let kind = match &event {
        Event::ItemUpserted(_) => "item_upserted",
        Event::PendingActionChanged(_) => "pending_action_changed",
        Event::RunStateChanged(_) => "run_state_changed",
        Event::ReadStateChanged(_) => "read_state_changed",
        Event::DraftChanged(_) => "draft_changed",
    };
    let envelope = EventEnvelope {
        seq: seq as u64,
        cause_cid: cid.into(),
        event: Some(event),
    };
    sqlx::query(
        "INSERT INTO events(channel_id,seq,org_id,kind,cause_cid,payload) VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(channel)
    .bind(seq)
    .bind(org)
    .bind(kind)
    .bind(Uuid::parse_str(cid).ok())
    .bind(envelope.encode_to_vec())
    .execute(&mut **tx)
    .await?;
    Ok(envelope)
}

/// Append an item revision to the lease's conversation channel and return it as committed.
pub async fn upsert_item(tx: &mut Tx<'_>, lease: &Lease, item: Item) -> anyhow::Result<Item> {
    let envelope = append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        "",
        Event::ItemUpserted(item),
    )
    .await?;
    match envelope.event {
        Some(Event::ItemUpserted(item)) => Ok(item),
        _ => unreachable!("append preserves the event kind"),
    }
}

impl Store {
    /// A transaction for the lease's organization that already holds the lease (see
    /// [`lock_lease`]). Every Owner write starts here.
    pub async fn fenced(&self, lease: &Lease) -> anyhow::Result<Tx<'_>> {
        let mut tx = self.tx(lease.org_id).await?;
        lock_lease(&mut tx, lease).await?;
        Ok(tx)
    }
}

/// Lock the lease's channel row, or fail with FENCED. Taken before any projection write.
pub async fn lock_lease(tx: &mut Tx<'_>, lease: &Lease) -> anyhow::Result<()> {
    let valid = sqlx::query_scalar!(
        "SELECT id FROM channels WHERE org_id=$1 AND id=$2 AND owner_id=$3 AND writer_epoch=$4
         AND lease_until>clock_timestamp() FOR UPDATE",
        lease.org_id,
        lease.channel(),
        lease.owner_id,
        lease.epoch
    )
    .fetch_optional(&mut **tx)
    .await?;
    ensure!(valid.is_some(), Code::Fenced);
    Ok(())
}

/// The channel's current head.
pub async fn head_seq(tx: &mut Tx<'_>, org: Uuid, channel: &str) -> anyhow::Result<u64> {
    let seq: i64 = sqlx::query_scalar("SELECT last_seq FROM channels WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(channel)
        .fetch_one(&mut **tx)
        .await?;
    Ok(seq as u64)
}

impl Store {
    /// Take over a conversation whose lease expired. Active runs and their pending actions move
    /// to the new execution epoch and are re-announced.
    pub async fn claim(
        &self,
        org: Uuid,
        conv: Uuid,
        owner: Uuid,
        ttl_ms: u64,
    ) -> anyhow::Result<Option<Lease>> {
        let mut tx = self.tx(org).await?;
        // A conversation whose channel row someone else holds right now is skipped, not waited
        // for: reconciliation tries it again shortly.
        let free: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM channels WHERE org_id=$1 AND conversation_id=$2 FOR UPDATE SKIP LOCKED",
        )
        .bind(org)
        .bind(conv)
        .fetch_optional(&mut *tx)
        .await?;
        if free.is_none() {
            tx.rollback().await?;
            return Ok(None);
        }
        let epoch = sqlx::query_scalar!(
            r#"UPDATE channels SET owner_id=$3,writer_epoch=writer_epoch+1,
                 lease_until=clock_timestamp()+($4::bigint * interval '1 millisecond')
             WHERE org_id=$1 AND conversation_id=$2 AND (lease_until IS NULL OR lease_until<clock_timestamp())
             RETURNING writer_epoch AS "writer_epoch!""#,
            org,
            conv,
            owner,
            ttl_ms as i64
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(epoch) = epoch else {
            tx.rollback().await?;
            return Ok(None);
        };
        let lease = Lease {
            org_id: org,
            conversation_id: conv,
            owner_id: owner,
            epoch,
        };
        sqlx::query("UPDATE conversations SET owner_epoch=$3 WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(conv)
            .bind(epoch)
            .execute(&mut *tx)
            .await?;
        type RunRow = (Uuid, String, bool, Option<i64>, Option<i64>);
        let runs: Vec<RunRow> = sqlx::query_as(concat!(
            "UPDATE runs SET exec_epoch=$3 WHERE org_id=$1 AND conversation_id=$2
             AND state IN ('running','requires_action') RETURNING id,state,read_only,",
            run_times!()
        ))
        .bind(org)
        .bind(conv)
        .bind(epoch)
        .fetch_all(&mut *tx)
        .await?;
        for (run, state, read_only, started_at_ms, ended_at_ms) in runs {
            append(
                &mut tx,
                org,
                &lease.channel(),
                Some(&lease),
                "",
                Event::RunStateChanged(RunState {
                    read_only,
                    run_id: run.to_string(),
                    state,
                    exec_epoch: epoch as u64,
                    started_at_ms: started_at_ms.unwrap_or_default(),
                    ended_at_ms: ended_at_ms.unwrap_or_default(),
                }),
            )
            .await?;
            let actions: Vec<ActionRow> = sqlx::query_as(concat!(
                "UPDATE pending_actions SET run_epoch=$3 WHERE org_id=$1 AND run_id=$2 AND status='pending' RETURNING ",
                action_columns!()
            ))
            .bind(org)
            .bind(run)
            .bind(epoch)
            .fetch_all(&mut *tx)
            .await?;
            for action in actions {
                append(
                    &mut tx,
                    org,
                    &lease.channel(),
                    Some(&lease),
                    "",
                    Event::PendingActionChanged(action.into()),
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(Some(lease))
    }

    pub async fn renew(&self, lease: &Lease, ttl_ms: u64) -> anyhow::Result<()> {
        let mut tx = self.tx(lease.org_id).await?;
        let renewed = sqlx::query!(
            "UPDATE channels SET lease_until=clock_timestamp()+($5::bigint * interval '1 millisecond') WHERE org_id=$1 AND id=$2 AND writer_epoch=$3 AND owner_id=$4 AND lease_until>clock_timestamp()",
            lease.org_id,
            lease.channel(),
            lease.epoch,
            lease.owner_id,
            ttl_ms as i64
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        ensure!(renewed == 1, Code::Fenced);
        tx.commit().await?;
        Ok(())
    }

    /// Committed events not yet confirmed as published, oldest first.
    pub async fn unpublished(
        &self,
        org: Uuid,
        channel: &str,
    ) -> anyhow::Result<Vec<EventEnvelope>> {
        let mut tx = self.tx(org).await?;
        let payloads: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT e.payload FROM events e JOIN channels c ON c.org_id=e.org_id AND c.id=e.channel_id
             WHERE e.org_id=$1 AND e.channel_id=$2 AND e.seq>c.published_seq ORDER BY e.seq LIMIT 256",
        )
        .bind(org)
        .bind(channel)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        payloads
            .into_iter()
            .map(|bytes| EventEnvelope::decode(bytes.as_slice()).map_err(Into::into))
            .collect()
    }

    /// Resolve a channel to its organization and check the user may subscribe to it.
    pub async fn authorize_channel(&self, user: Uuid, channel: &str) -> anyhow::Result<OrgContext> {
        let org = self.channel_org(user, channel).await?;
        let ctx = self.context(user, org).await?;
        authorize(
            &Principal::User(ctx.clone()),
            Action::Subscribe,
            &Resource {
                org_id: org,
                id: org,
                owner_user_id: Some(user),
                target_role: None,
            },
        )
        .map_err(|_| Code::Forbidden)?;
        Ok(ctx)
    }

    /// The organization of a channel `user` may name: one of their conversations, their user
    /// channel or an organization channel. Membership is for the caller to check.
    pub async fn channel_org(&self, user: Uuid, channel: &str) -> anyhow::Result<Uuid> {
        let parts: Vec<_> = channel.split(':').collect();
        let org = match parts.as_slice() {
            ["conv", id] => {
                let conv = Uuid::parse_str(id).context(Code::InvalidChannel)?;
                let org: Option<Uuid> =
                    sqlx::query_scalar("SELECT resolve_conversation_org($1,$2)")
                        .bind(user)
                        .bind(conv)
                        .fetch_one(&self.pool)
                        .await?;
                org.context(Code::Forbidden)?
            }
            ["user", org, id] => {
                ensure!(Uuid::parse_str(id).ok() == Some(user), Code::Forbidden);
                Uuid::parse_str(org).context(Code::InvalidChannel)?
            }
            // TODO(org): subscribable, though nothing is published on it yet.
            ["org", org] => Uuid::parse_str(org).context(Code::InvalidChannel)?,
            _ => anyhow::bail!(Code::InvalidChannel),
        };
        Ok(org)
    }

    /// Events after `since` if still retained, otherwise a snapshot. Returns the head seq the
    /// frames bring a subscriber to. Runs in one REPEATABLE READ transaction.
    pub async fn replay(
        &self,
        ctx: &OrgContext,
        channel: &str,
        since: Option<u64>,
    ) -> anyhow::Result<(Vec<Frame>, u64)> {
        self.authorize_channel(ctx.user_id, channel).await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT set_config('app.org_id',$1,true)")
            .bind(ctx.org_id.to_string())
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query(
            "SELECT last_seq,kind,conversation_id FROM channels WHERE org_id=$1 AND id=$2",
        )
        .bind(ctx.org_id)
        .bind(channel)
        .fetch_one(&mut *tx)
        .await?;
        let head = row.get::<i64, _>("last_seq") as u64;
        let kind: String = row.get("kind");
        let conversation: Option<Uuid> = row.get("conversation_id");
        let frames = match since {
            Some(since) if since == head => vec![],
            Some(since)
                if since < head && retained(&mut tx, ctx.org_id, channel, since).await? =>
            {
                events_between(&mut tx, ctx.org_id, channel, since, head).await?
            }
            _ => {
                let snapshot = match (kind.as_str(), conversation) {
                    ("conv", Some(conv)) => {
                        conversation_snapshot(&mut tx, ctx.org_id, conv).await?
                    }
                    ("user", _) => user_snapshot(&mut tx, ctx).await?,
                    _ => Snapshot::default(),
                };
                vec![frame(
                    channel,
                    Body::Snapshot(Snapshot {
                        head_seq: head,
                        ..snapshot
                    }),
                )]
            }
        };
        tx.commit().await?;
        Ok((frames, head))
    }

    /// One page of items older than `before` (newest first) and whether more remain.
    pub async fn history(
        &self,
        ctx: &OrgContext,
        conv: Uuid,
        before: i64,
    ) -> anyhow::Result<(Vec<Item>, bool)> {
        const PAGE: usize = 100;
        let mut tx = self.tx(ctx.org_id).await?;
        super::conversation_in(&mut tx, ctx, conv).await?;
        // One extra row tells whether anything older remains.
        let mut items: Vec<ItemRow> = sqlx::query_as(concat!(
            "SELECT ",
            item_columns!(),
            " FROM items WHERE org_id=$1 AND conversation_id=$2 AND first_seq<$3 ORDER BY first_seq DESC LIMIT $4"
        ))
        .bind(ctx.org_id)
        .bind(conv)
        .bind(before)
        .bind(PAGE as i64 + 1)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let more = items.len() > PAGE;
        items.truncate(PAGE);
        Ok((items.into_iter().map(Into::into).collect(), more))
    }
}

async fn retained(tx: &mut Tx<'_>, org: Uuid, channel: &str, since: u64) -> anyhow::Result<bool> {
    let oldest: Option<i64> =
        sqlx::query_scalar("SELECT min(seq) FROM events WHERE org_id=$1 AND channel_id=$2")
            .bind(org)
            .bind(channel)
            .fetch_one(&mut **tx)
            .await?;
    Ok(oldest.is_some_and(|oldest| since + 1 >= oldest as u64))
}

/// Durable events in `(after, through]` as frames.
pub async fn events_between(
    tx: &mut Tx<'_>,
    org: Uuid,
    channel: &str,
    after: u64,
    through: u64,
) -> anyhow::Result<Vec<Frame>> {
    let payloads: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT payload FROM events WHERE org_id=$1 AND channel_id=$2 AND seq>$3 AND seq<=$4 ORDER BY seq",
    )
    .bind(org)
    .bind(channel)
    .bind(after as i64)
    .bind(through as i64)
    .fetch_all(&mut **tx)
    .await?;
    payloads
        .into_iter()
        .map(|bytes| {
            Ok(frame(
                channel,
                Body::Event(EventEnvelope::decode(bytes.as_slice())?),
            ))
        })
        .collect()
}

async fn conversation_snapshot(tx: &mut Tx<'_>, org: Uuid, conv: Uuid) -> anyhow::Result<Snapshot> {
    let items: Vec<ItemRow> = sqlx::query_as(concat!(
        "SELECT ",
        item_columns!(),
        " FROM items WHERE org_id=$1 AND conversation_id=$2 ORDER BY first_seq DESC LIMIT $3"
    ))
    .bind(org)
    .bind(conv)
    .bind(SNAPSHOT_ITEMS)
    .fetch_all(&mut **tx)
    .await?;
    let actions: Vec<ActionRow> = sqlx::query_as(
        "SELECT a.id,a.run_id,a.run_epoch,a.kind,a.summary,a.options,a.status,a.resolved_device,a.expires_at,a.form
         FROM pending_actions a JOIN runs r ON r.org_id=a.org_id AND r.id=a.run_id
         WHERE a.org_id=$1 AND r.conversation_id=$2 AND a.status='pending'",
    )
    .bind(org)
    .bind(conv)
    .fetch_all(&mut **tx)
    .await?;
    type RunRow = (Uuid, String, i64, bool, Option<i64>, Option<i64>);
    let runs: Vec<RunRow> = sqlx::query_as(concat!(
        "SELECT id,state,exec_epoch,read_only,",
        run_times!(),
        " FROM runs WHERE org_id=$1 AND conversation_id=$2 ORDER BY started_at DESC LIMIT 50"
    ))
    .bind(org)
    .bind(conv)
    .fetch_all(&mut **tx)
    .await?;
    Ok(Snapshot {
        items: items.into_iter().map(Into::into).collect(),
        pending_actions: actions.into_iter().map(Into::into).collect(),
        runs: runs
            .into_iter()
            .map(
                |(id, state, epoch, read_only, started_at_ms, ended_at_ms)| RunState {
                    read_only,
                    run_id: id.to_string(),
                    state,
                    exec_epoch: epoch as u64,
                    started_at_ms: started_at_ms.unwrap_or_default(),
                    ended_at_ms: ended_at_ms.unwrap_or_default(),
                },
            )
            .collect(),
        ..Default::default()
    })
}

async fn user_snapshot(tx: &mut Tx<'_>, ctx: &OrgContext) -> anyhow::Result<Snapshot> {
    let conversations: Vec<Conversation> = sqlx::query_as(concat!(
        "SELECT ",
        conversation_columns!(),
        " FROM conversations WHERE org_id=$1 AND creator_id=$2 AND state IN ('active','archived')
          ORDER BY pinned_at DESC NULLS LAST, created_at DESC LIMIT 200"
    ))
    .bind(ctx.org_id)
    .bind(ctx.user_id)
    .fetch_all(&mut **tx)
    .await?;
    let read_states: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT conversation_id,read_seq FROM read_states WHERE org_id=$1 AND user_id=$2",
    )
    .bind(ctx.org_id)
    .bind(ctx.user_id)
    .fetch_all(&mut **tx)
    .await?;
    let drafts = crate::drafts::of_user(tx, ctx).await?;
    Ok(Snapshot {
        drafts,
        items: conversations
            .into_iter()
            .map(|c| conversation_item(&c, 1))
            .collect(),
        read_states: read_states
            .into_iter()
            .map(|(id, seq)| ReadState {
                user_id: ctx.user_id.to_string(),
                conversation_id: id.to_string(),
                read_seq: seq as u64,
            })
            .collect(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_character_boundaries_and_bounds_details() {
        let mut text = "你好".repeat(10);
        assert!(truncate_utf8(&mut text, 7));
        assert_eq!(text, "你好");
        let mut item = Item {
            text: "x".repeat(MAX_ITEM_TEXT + 1),
            detail: Some(ItemDetail {
                kind: Some(item_detail::Kind::Tool(ToolDetail {
                    output: "y".repeat(MAX_DETAIL_FIELD * 2),
                    ..Default::default()
                })),
            }),
            ..Default::default()
        };
        bound_item(&mut item);
        assert!(item.truncated);
        assert_eq!(item.text.len(), MAX_ITEM_TEXT);
        let Some(item_detail::Kind::Tool(tool)) = item.detail.unwrap().kind else {
            unreachable!()
        };
        assert_eq!(tool.output.len(), MAX_DETAIL_FIELD);
    }
}
