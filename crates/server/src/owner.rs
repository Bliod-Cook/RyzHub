//! A single actor publishes each conversation's durable events and ephemeral deltas.
use crate::{config::Config, store::*};
use anyhow::{Context, ensure};
use futures_util::StreamExt;
use prost::Message;
use ryzhub_proto::{command_envelope::Command, event_envelope::Event, frame::Body, *};
use sqlx::{Acquire, PgPool, Row};
use std::{collections::HashMap, time::Duration};
use tokio::task::JoinHandle;
use uuid::Uuid;

pub fn subject(channel: &str) -> String {
    format!("ryzhub.events.{}", channel.replace(':', "."))
}
pub fn fold_subject(channel: &str) -> String {
    format!("ryzhub.fold.{}", channel.replace(':', "."))
}

pub async fn run(
    store: Store,
    worker: PgPool,
    nats: async_nats::Client,
    config: Config,
) -> anyhow::Result<()> {
    let owner = Uuid::now_v7();
    let mut actors: HashMap<Uuid, ActorTask> = HashMap::new();
    let mut timer = tokio::time::interval(Duration::from_millis(100));
    loop {
        timer.tick().await;
        actors.retain(|_, task| !task.0.is_finished());
        let rows:Vec<(Uuid,Uuid)>=sqlx::query_as("SELECT c.org_id,c.conversation_id FROM channels c WHERE c.kind='conv' AND (c.last_seq>c.published_seq OR EXISTS(SELECT 1 FROM commands q WHERE q.org_id=c.org_id AND q.conversation_id=c.conversation_id AND q.status='queued') OR EXISTS(SELECT 1 FROM runs r WHERE r.org_id=c.org_id AND r.conversation_id=c.conversation_id AND r.state IN ('running','requires_action')) OR EXISTS(SELECT 1 FROM workspace_jobs j WHERE j.org_id=c.org_id AND j.conversation_id=c.conversation_id AND j.state IN ('queued','running')) OR EXISTS(SELECT 1 FROM sandboxes s WHERE s.org_id=c.org_id AND s.conversation_id=c.conversation_id AND s.state IN ('running','provisioning')))")
            .fetch_all(&worker).await?;
        for (org, conv) in rows {
            if actors.contains_key(&conv) {
                continue;
            }
            if let Some(lease) = store.claim(org, conv, owner, config.owner_lease_ms).await? {
                let store = store.clone();
                let nats = nats.clone();
                let config = config.clone();
                actors.insert(
                    conv,
                    ActorTask(tokio::spawn(async move {
                        if let Err(error) = actor(store, nats, config, lease).await {
                            tracing::warn!(%conv,%error,"owner actor stopped");
                        }
                    })),
                );
            }
        }
        let nonconv: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT org_id,id FROM channels WHERE kind<>'conv' AND last_seq>published_seq",
        )
        .fetch_all(&worker)
        .await?;
        for (org, channel) in nonconv {
            publish(&store, &nats, org, &channel, None, vec![]).await?;
        }
    }
}

struct ActorTask(JoinHandle<()>);
impl Drop for ActorTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Live {
    item: Item,
    full: String,
    offset: usize,
}
impl Live {
    fn fold(&self, channel: &str) -> Frame {
        frame(
            channel,
            Body::ItemFold(ItemFold {
                item_id: self.item.id.clone(),
                stream: "text".into(),
                part_index: 0,
                text: self.full[..self.offset].into(),
                base_seq: self.item.last_seq,
                revision: self.item.revision,
            }),
        )
    }
}
async fn actor(
    store: Store,
    nats: async_nats::Client,
    config: Config,
    lease: Lease,
) -> anyhow::Result<()> {
    if config.llm.enabled {
        let mut tx = store.tx(lease.org_id).await?;
        let mode: String =
            sqlx::query_scalar("SELECT mode FROM conversations WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(lease.conversation_id)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        if mode == "chat" || (mode == "work" && config.work.enabled) {
            return crate::chat::actor(store, nats, config, lease).await;
        }
    }
    let channel = lease.channel();
    let mut requests = nats.subscribe(fold_subject(&channel)).await?;
    let mut timer = tokio::time::interval(Duration::from_millis(config.mock_chunk_ms));
    let mut live: Option<Live> = None;
    let mut idle = 0;
    loop {
        tokio::select! {
            _=timer.tick()=>{
                store.renew(&lease,config.owner_lease_ms).await?;
                for _ in 0..16 {if !process_with_config(&store,&lease,&config).await? {break;}}
                let mut tx=store.tx(lease.org_id).await?;
                let unauthorized:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM conversations c JOIN memberships m ON m.org_id=c.org_id AND m.user_id=c.creator_id WHERE c.org_id=$1 AND c.id=$2)").bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut *tx).await?;
                let expired:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pending_actions a JOIN runs r ON r.org_id=a.org_id AND r.id=a.run_id WHERE a.org_id=$1 AND r.conversation_id=$2 AND a.status='pending' AND a.expires_at<=now())").bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut *tx).await?;
                if unauthorized || expired {cancel(&mut tx,&lease,"").await?;}
                let row=sqlx::query("SELECT r.id,r.response_text,r.assistant_item_id,i.revision,i.first_seq,i.last_seq FROM runs r JOIN items i ON i.org_id=r.org_id AND i.id=r.assistant_item_id JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id WHERE r.org_id=$1 AND r.conversation_id=$2 AND r.state='running' AND EXISTS(SELECT 1 FROM memberships m WHERE m.org_id=c.org_id AND m.user_id=c.creator_id)")
                    .bind(lease.org_id).bind(lease.conversation_id).fetch_optional(&mut *tx).await?;
                tx.commit().await?;
                let mut transient=vec![];
                if let Some(row)=row {
                    idle=0;
                    let item_id:Uuid=row.get("assistant_item_id");
                    if live.as_ref().is_none_or(|l|l.item.id!=item_id.to_string()) {
                        live=Some(Live {item:Item {id:item_id.to_string(),revision:row.get::<i64,_>("revision") as u64,kind:"assistant".into(),state:"streaming".into(),first_seq:row.get::<i64,_>("first_seq") as u64,last_seq:row.get::<i64,_>("last_seq") as u64,run_id:row.get::<Uuid,_>("id").to_string(),text:String::new()},full:row.get("response_text"),offset:0});
                    }
                    let l=live.as_mut().unwrap();
                    if l.offset<l.full.len() {
                        let start=l.offset;
                        l.offset=l.full[start..].char_indices().nth(4).map(|(i,_)|start+i).unwrap_or(l.full.len());
                        transient.push(frame(&channel,Body::Delta(Delta {item_id:l.item.id.clone(),stream:"text".into(),part_index:0,offset:start as u64,text:l.full[start..l.offset].into(),base_seq:l.item.last_seq,revision:l.item.revision})));
                    } else {
                        let mut tx=store.tx(lease.org_id).await?;
                        l.item.revision+=1;l.item.state="completed".into();l.item.text=l.full.clone();
                        append(&mut tx,lease.org_id,&channel,Some(&lease),"",Event::ItemUpserted(l.item.clone())).await?;
                        let run=Uuid::parse_str(&l.item.run_id)?;
                        sqlx::query("UPDATE runs SET state='completed',ended_at=now() WHERE org_id=$1 AND id=$2 AND exec_epoch=$3").bind(lease.org_id).bind(run).bind(lease.epoch).execute(&mut *tx).await?;
                        append(&mut tx,lease.org_id,&channel,Some(&lease),"",Event::RunStateChanged(RunState { read_only: false,run_id:run.to_string(),state:"completed".into(),exec_epoch:lease.epoch as u64})).await?;
                        tx.commit().await?;live=None;
                    }
                } else {live=None;idle+=1;}
                publish(&store,&nats,lease.org_id,&channel,Some(&lease),transient).await?;
                if idle>250 {return Ok(());}
            }
            Some(request)=requests.next()=>{
                store.renew(&lease,config.owner_lease_ms).await?;
                if let Some(reply)=request.reply {
                    let batch=FrameBatch {frames:live.as_ref().map(|l|{let mut f=l.fold(&channel);f.writer_epoch=lease.epoch as u64;vec![f]}).unwrap_or_default()};
                    nats.publish(reply,batch.encode_to_vec().into()).await?;
                }
            }
        }
    }
}

/// Hold the channel row lock across publish + flush. A takeover cannot publish while
/// the old publisher is still in its ordered critical section. Crash => replay.
pub async fn publish(
    store: &Store,
    nats: &async_nats::Client,
    org: Uuid,
    channel: &str,
    lease: Option<&Lease>,
    transient: Vec<Frame>,
) -> anyhow::Result<()> {
    let mut tx = store.tx(org).await?;
    let row=sqlx::query("SELECT last_seq,published_seq FROM channels WHERE org_id=$1 AND id=$2 AND (($3::bigint IS NULL AND writer_epoch IS NULL) OR (writer_epoch=$3 AND owner_id=$4 AND lease_until>clock_timestamp())) FOR UPDATE")
        .bind(org).bind(channel).bind(lease.map(|l|l.epoch)).bind(lease.map(|l|l.owner_id)).fetch_optional(&mut *tx).await?.context("FENCED")?;
    let head: i64 = row.get("last_seq");
    let published: i64 = row.get("published_seq");
    let payloads: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT payload FROM events WHERE org_id=$1 AND channel_id=$2 AND seq>$3 ORDER BY seq",
    )
    .bind(org)
    .bind(channel)
    .bind(published)
    .fetch_all(&mut *tx)
    .await?;
    for payload in payloads {
        let mut f = frame(
            channel,
            Body::Event(EventEnvelope::decode(payload.as_slice())?),
        );
        f.writer_epoch = lease.map_or(0, |l| l.epoch as u64);
        nats.publish(subject(channel), f.encode_to_vec().into())
            .await?;
    }
    for mut f in transient {
        f.writer_epoch = lease.map_or(0, |l| l.epoch as u64);
        nats.publish(subject(channel), f.encode_to_vec().into())
            .await?;
    }
    tokio::time::timeout(Duration::from_millis(750), nats.flush()).await??;
    sqlx::query("UPDATE channels SET published_seq=$3 WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(channel)
        .bind(head)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn process_command(store: &Store, lease: &Lease) -> anyhow::Result<bool> {
    process_with_config(store, lease, &Config::default()).await
}
pub async fn process_with_config(
    store: &Store,
    lease: &Lease,
    config: &Config,
) -> anyhow::Result<bool> {
    let mut tx = store.tx(lease.org_id).await?;
    // Lock the same row used by fencing BEFORE touching commands or projections.
    let valid:Option<String>=sqlx::query_scalar("SELECT id FROM channels WHERE org_id=$1 AND id=$2 AND owner_id=$3 AND writer_epoch=$4 AND lease_until>clock_timestamp() FOR UPDATE")
        .bind(lease.org_id).bind(lease.channel()).bind(lease.owner_id).bind(lease.epoch).fetch_optional(&mut *tx).await?;
    ensure!(valid.is_some(), "FENCED");
    let row=sqlx::query("SELECT id,user_id,device_id,payload FROM commands WHERE org_id=$1 AND conversation_id=$2 AND status='queued' ORDER BY created_at,id LIMIT 1 FOR UPDATE")
        .bind(lease.org_id).bind(lease.conversation_id).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(false);
    };
    let cmd = CommandEnvelope::decode(row.get::<Vec<u8>, _>("payload").as_slice())?;
    let user: Uuid = row.get("user_id");
    let device: String = row.get("device_id");
    let command_id: Uuid = row.get("id");
    let mut savepoint = tx.begin().await?;
    let result = execute(&mut savepoint, lease, user, &device, &cmd, config).await;
    let response = match result {
        Ok(body) => {
            savepoint.commit().await?;
            frame(lease.channel(), body)
        }
        Err(error) => {
            savepoint.rollback().await?;
            if error.downcast_ref::<sqlx::Error>().is_some() || error.to_string() == "FENCED" {
                return Err(error);
            }
            frame(
                lease.channel(),
                Body::CommandRejected(CommandRejected {
                    cid: cmd.cid.clone(),
                    code: error.to_string(),
                    retryable: false,
                }),
            )
        }
    };
    sqlx::query("UPDATE commands SET status='done',result=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(command_id)
        .bind(response.encode_to_vec())
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO audit_logs(id,org_id,actor_id,actor_kind,action,resource,result) VALUES($1,$2,$3,'user','command',$4,$5)").bind(Uuid::now_v7()).bind(lease.org_id).bind(user).bind(lease.channel()).bind(if matches!(response.body,Some(Body::CommandAck(_))){"accepted"}else{"rejected"}).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}
async fn execute(
    tx: &mut Tx<'_>,
    lease: &Lease,
    user: Uuid,
    device: &str,
    cmd: &CommandEnvelope,
    config: &Config,
) -> anyhow::Result<Body> {
    let ctx = context_in(tx, user, lease.org_id).await?;
    let conv = conversation_in(tx, &ctx, lease.conversation_id).await?;
    ensure!(conv.state == "active", "CONVERSATION_ARCHIVED");
    let channel = lease.channel();
    if config.llm.enabled
        && (conv.mode == "chat" || (conv.mode == "work" && config.work.enabled))
        && !matches!(cmd.command, Some(Command::MarkRead(_)))
    {
        return crate::chat::commands::execute(tx, lease, user, device, cmd, config).await;
    }
    let policy = &config.policy;
    match cmd.command.as_ref().context("INVALID_COMMAND")? {
        Command::SendMessage(msg) => {
            // Serialize org-wide concurrency decisions across conversation actors.
            sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
                .bind(lease.org_id)
                .fetch_one(&mut **tx)
                .await?;
            let org_policy: Option<serde_json::Value> = sqlx::query_scalar(
                "SELECT policy FROM org_policies WHERE org_id=$1 ORDER BY version DESC LIMIT 1",
            )
            .bind(lease.org_id)
            .fetch_optional(&mut **tx)
            .await?;
            let effective = policy.restrict(&serde_json::from_value(
                org_policy.unwrap_or_else(|| serde_json::json!({})),
            )?);
            ensure!(
                effective.models.as_ref().is_none_or(|m| m.contains("mock"))
                    && effective
                        .harnesses
                        .as_ref()
                        .is_none_or(|h| h.contains("mock")),
                "POLICY_DENIED"
            );
            if let Some(limit) = effective.max_sandboxes {
                let active:i64=sqlx::query_scalar("SELECT count(*) FROM runs WHERE org_id=$1 AND state IN ('running','requires_action')").bind(lease.org_id).fetch_one(&mut **tx).await?;
                ensure!(active < limit as i64, "ORG_CONCURRENCY_LIMIT");
            }
            ensure!(
                !msg.text.trim().is_empty() && msg.text.len() <= 4096,
                "INVALID_MESSAGE"
            );
            let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('running','requires_action'))").bind(lease.org_id).bind(lease.conversation_id).fetch_one(&mut **tx).await?;
            ensure!(!busy, "RUN_BUSY");
            let run = Uuid::now_v7();
            let item = Uuid::now_v7();
            let needs_approval = msg.text.starts_with("/approve");
            let state = if needs_approval {
                "requires_action"
            } else {
                "running"
            };
            sqlx::query("INSERT INTO runs(id,org_id,conversation_id,exec_epoch,state,response_text,assistant_item_id,cause_cid) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
                .bind(run).bind(lease.org_id).bind(lease.conversation_id).bind(lease.epoch).bind(state).bind(format!("模拟 agent 已收到：{}\n\n这是 M0 的可恢复流式响应。你好，Rust 🦀。",msg.text)).bind(item).bind(Uuid::parse_str(&cmd.cid)?).execute(&mut **tx).await?;
            append(
                tx,
                lease.org_id,
                &channel,
                Some(lease),
                &cmd.cid,
                Event::ItemUpserted(Item {
                    id: Uuid::now_v7().to_string(),
                    revision: 1,
                    kind: "user".into(),
                    state: "completed".into(),
                    text: msg.text.clone(),
                    run_id: run.to_string(),
                    ..Default::default()
                }),
            )
            .await?;
            append(
                tx,
                lease.org_id,
                &channel,
                Some(lease),
                &cmd.cid,
                Event::ItemUpserted(Item {
                    id: item.to_string(),
                    revision: 1,
                    kind: "assistant".into(),
                    state: "streaming".into(),
                    run_id: run.to_string(),
                    ..Default::default()
                }),
            )
            .await?;
            append(
                tx,
                lease.org_id,
                &channel,
                Some(lease),
                &cmd.cid,
                Event::RunStateChanged(RunState {
                    read_only: false,
                    run_id: run.to_string(),
                    state: state.into(),
                    exec_epoch: lease.epoch as u64,
                }),
            )
            .await?;
            if needs_approval {
                let action:ActionRow=sqlx::query_as("INSERT INTO pending_actions(id,org_id,run_id,run_epoch,summary,options,expires_at) VALUES($1,$2,$3,$4,'允许模拟工具继续？',ARRAY['allow','deny'],now()+interval '10 minutes') RETURNING id,run_id,run_epoch,summary,options,status,resolved_device,expires_at")
                    .bind(Uuid::now_v7()).bind(lease.org_id).bind(run).bind(lease.epoch).fetch_one(&mut **tx).await?;
                append(
                    tx,
                    lease.org_id,
                    &channel,
                    Some(lease),
                    &cmd.cid,
                    Event::PendingActionChanged(action.into()),
                )
                .await?;
            }
        }
        Command::DecideApproval(decision) => {
            ensure!(
                matches!(decision.decision.as_str(), "allow" | "deny"),
                "INVALID_DECISION"
            );
            let id = Uuid::parse_str(&decision.action_id).context("INVALID_ACTION")?;
            let action:Option<ActionRow>=sqlx::query_as("UPDATE pending_actions a SET status=$5,resolved_by=$6,resolved_device=$7 FROM runs r WHERE a.org_id=$1 AND a.id=$2 AND a.run_epoch=$3 AND a.status='pending' AND a.expires_at>now() AND r.org_id=a.org_id AND r.id=a.run_id AND r.conversation_id=$4 AND r.exec_epoch=$3 AND r.state='requires_action' RETURNING a.id,a.run_id,a.run_epoch,a.summary,a.options,a.status,a.resolved_device,a.expires_at")
                .bind(lease.org_id).bind(id).bind(decision.run_epoch as i64).bind(lease.conversation_id).bind(&decision.decision).bind(user).bind(device).fetch_optional(&mut **tx).await?;
            let action = action.context("ACTION_ALREADY_RESOLVED")?;
            let run = action.run_id;
            append(
                tx,
                lease.org_id,
                &channel,
                Some(lease),
                &cmd.cid,
                Event::PendingActionChanged(action.into()),
            )
            .await?;
            if decision.decision == "allow" {
                sqlx::query("UPDATE runs SET state='running' WHERE org_id=$1 AND id=$2")
                    .bind(lease.org_id)
                    .bind(run)
                    .execute(&mut **tx)
                    .await?;
                append(
                    tx,
                    lease.org_id,
                    &channel,
                    Some(lease),
                    &cmd.cid,
                    Event::RunStateChanged(RunState {
                        read_only: false,
                        run_id: run.to_string(),
                        state: "running".into(),
                        exec_epoch: lease.epoch as u64,
                    }),
                )
                .await?;
            } else {
                cancel(tx, lease, &cmd.cid).await?;
            }
        }
        Command::Cancel(_) => {
            cancel(tx, lease, &cmd.cid).await?;
        }
        Command::MarkRead(read) => {
            let head: i64 =
                sqlx::query_scalar("SELECT last_seq FROM channels WHERE org_id=$1 AND id=$2")
                    .bind(lease.org_id)
                    .bind(&channel)
                    .fetch_one(&mut **tx)
                    .await?;
            ensure!(read.read_seq <= head as u64, "INVALID_READ_SEQ");
            let seq:i64=sqlx::query_scalar("INSERT INTO read_states(org_id,user_id,conversation_id,read_seq) VALUES($1,$2,$3,$4) ON CONFLICT(user_id,conversation_id) DO UPDATE SET read_seq=greatest(read_states.read_seq,EXCLUDED.read_seq) RETURNING read_seq")
                .bind(lease.org_id).bind(user).bind(lease.conversation_id).bind(read.read_seq as i64).fetch_one(&mut **tx).await?;
            append(
                tx,
                lease.org_id,
                &format!("user:{}:{user}", lease.org_id),
                None,
                &cmd.cid,
                Event::ReadStateChanged(ReadState {
                    user_id: user.to_string(),
                    conversation_id: lease.conversation_id.to_string(),
                    read_seq: seq as u64,
                }),
            )
            .await?;
        }
        _ => anyhow::bail!("UNSUPPORTED_COMMAND"),
    }
    let seq: i64 = sqlx::query_scalar("SELECT last_seq FROM channels WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(channel)
        .fetch_one(&mut **tx)
        .await?;
    Ok(Body::CommandAck(CommandAck {
        cid: cmd.cid.clone(),
        accepted_seq: seq as u64,
        result: "accepted".into(),
    }))
}
pub async fn cancel(tx: &mut Tx<'_>, lease: &Lease, cid: &str) -> anyhow::Result<()> {
    let run:Option<(Uuid,Uuid)>=sqlx::query_as("UPDATE runs SET state='cancelled',ended_at=now() WHERE org_id=$1 AND conversation_id=$2 AND state IN ('running','requires_action') RETURNING id,assistant_item_id").bind(lease.org_id).bind(lease.conversation_id).fetch_optional(&mut **tx).await?;
    if let Some((run, item)) = run {
        let row =
            sqlx::query("SELECT revision,first_seq,text FROM items WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(item)
                .fetch_one(&mut **tx)
                .await?;
        append(
            tx,
            lease.org_id,
            &lease.channel(),
            Some(lease),
            cid,
            Event::ItemUpserted(Item {
                id: item.to_string(),
                revision: row.get::<i64, _>("revision") as u64 + 1,
                kind: "assistant".into(),
                state: "cancelled".into(),
                text: row.get("text"),
                first_seq: row.get::<i64, _>("first_seq") as u64,
                last_seq: 0,
                run_id: run.to_string(),
            }),
        )
        .await?;
        let actions:Vec<ActionRow>=sqlx::query_as("UPDATE pending_actions SET status='invalidated' WHERE org_id=$1 AND run_id=$2 AND status='pending' RETURNING id,run_id,run_epoch,summary,options,status,resolved_device,expires_at").bind(lease.org_id).bind(run).fetch_all(&mut **tx).await?;
        for action in actions {
            append(
                tx,
                lease.org_id,
                &lease.channel(),
                Some(lease),
                cid,
                Event::PendingActionChanged(action.into()),
            )
            .await?;
        }
        append(
            tx,
            lease.org_id,
            &lease.channel(),
            Some(lease),
            cid,
            Event::RunStateChanged(RunState {
                read_only: false,
                run_id: run.to_string(),
                state: "cancelled".into(),
                exec_epoch: lease.epoch as u64,
            }),
        )
        .await?;
    }
    Ok(())
}
