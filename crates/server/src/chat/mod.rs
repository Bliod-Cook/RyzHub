pub mod commands;
pub(crate) mod protocol;
pub(crate) mod runner;
mod tools;

use crate::{
    api::App,
    config::Config,
    owner,
    store::{ActionRow, Lease, Store, Tx, append},
};
use anyhow::{Context, ensure};
use futures_util::StreamExt;
use prost::Message;
use ryzhub_proto::{event_envelope::Event, frame::Body, *};
use sqlx::Row;
use std::{collections::HashMap, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle};
use uuid::Uuid;

pub struct Progress {
    pub item: Item,
    pub text: String,
    pub offset: usize,
}
struct Worker(JoinHandle<anyhow::Result<()>>);
impl Drop for Worker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn actor(
    store: Store,
    nats: async_nats::Client,
    config: Config,
    lease: Lease,
) -> anyhow::Result<()> {
    let mut tx = store.tx(lease.org_id).await?;
    let work: bool =
        sqlx::query_scalar("SELECT mode='work' FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    let app = App {
        store: store.clone(),
        nats: nats.clone(),
        config: config.clone(),
    };
    let mut folds = nats
        .subscribe(owner::fold_subject(&lease.channel()))
        .await?;
    let (sender, mut receiver) = mpsc::channel::<Progress>(64);
    let mut workers: HashMap<Uuid, Worker> = HashMap::new();
    let mut live: HashMap<String, ItemFold> = HashMap::new();
    let mut timer = tokio::time::interval(Duration::from_millis(100));
    let mut idle = 0;
    let mut cleanup: Option<Worker> = None;
    loop {
        tokio::select! {
            _=timer.tick()=>{
                store.renew(&lease,config.owner_lease_ms).await?;
                if cleanup.as_ref().is_none_or(|w|w.0.is_finished()) {
                    let app=app.clone();let lease=lease.clone();
                    cleanup=Some(Worker(tokio::spawn(async move {crate::conversation_commands::jobs(&app,&lease).await?;if work {crate::checkpoint::jobs(&app,&lease).await?;let _=crate::work::interrupt_terminal(&app,&lease).await;}crate::sandbox::cleanup(&app,&lease).await})));
                }
                for _ in 0..16 {if !owner::process_with_config(&store,&lease,&config).await? {break;}}
                let mut tx=store.tx(lease.org_id).await?;
                lock(&mut tx,&lease).await?;
                let invalid:Vec<Uuid>=sqlx::query_scalar("SELECT r.id FROM runs r JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id WHERE r.org_id=$1 AND r.conversation_id=$2 AND r.state IN ('running','requires_action') AND (NOT EXISTS(SELECT 1 FROM memberships m WHERE m.org_id=c.org_id AND m.user_id=c.creator_id) OR EXISTS(SELECT 1 FROM pending_actions a WHERE a.org_id=r.org_id AND a.run_id=r.id AND a.status='pending' AND a.expires_at<=now()))").bind(lease.org_id).bind(lease.conversation_id).fetch_all(&mut *tx).await?;
                for run in invalid {finish(&mut tx,&lease,run,"cancelled",Some("AUTHORIZATION_OR_ACTION_EXPIRED")).await?;}
                let rows:Vec<(Uuid,String,bool)>=sqlx::query_as("SELECT id,state,read_only FROM runs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('running','requires_action')").bind(lease.org_id).bind(lease.conversation_id).fetch_all(&mut *tx).await?;
                tx.commit().await?;
                workers.retain(|run,_|rows.iter().any(|(id,state,_)|id==run && (state=="running" || work)));
                for (run,state,read_only) in &rows {
                    if state!="running" && !work {continue;}
                    if workers.get(run).is_some_and(|w|w.0.is_finished()) {
                        let mut worker=workers.remove(run).unwrap();
                        if let Ok(Err(error))=(&mut worker.0).await {
                            if error.to_string()!="FENCED" {
                                tracing::warn!(%run,%error,"chat run failed");
                                let mut tx=store.tx(lease.org_id).await?;lock(&mut tx,&lease).await?;
                                finish(&mut tx,&lease,*run,"failed",Some(&error.to_string())).await?;tx.commit().await?;
                            }
                            continue;
                        }
                    }
                    if !workers.contains_key(run) {
                        let app=app.clone();let lease=lease.clone();let sender=sender.clone();let run=*run;let read_only=*read_only;
                        workers.insert(run,Worker(tokio::spawn(async move {if work && !read_only { crate::work::drive(app,lease,run,sender).await } else { runner::drive(app,lease,run,sender).await }})));
                    }
                }
                let mut tx=store.tx(lease.org_id).await?;
                let streaming:Vec<String>=sqlx::query_scalar("SELECT id::text FROM items WHERE org_id=$1 AND conversation_id=$2 AND state='streaming'").bind(lease.org_id).bind(lease.conversation_id).fetch_all(&mut *tx).await?;tx.commit().await?;
                live.retain(|id,_|streaming.contains(id));
                owner::publish(&store,&nats,lease.org_id,&lease.channel(),Some(&lease),vec![]).await?;
                if rows.is_empty(){idle+=1;}else{idle=0;}
                if idle>100 && cleanup.as_ref().is_none_or(|w|w.0.is_finished()) {return Ok(());}
            }
            Some(progress)=receiver.recv()=>{
                let run=Uuid::parse_str(&progress.item.run_id)?;
                let mut tx=store.tx(lease.org_id).await?;
                lock(&mut tx,&lease).await?;
                let valid=sqlx::query("UPDATE runs r SET response_text=$4 WHERE org_id=$1 AND id=$2 AND exec_epoch=$3 AND state='running' AND EXISTS(SELECT 1 FROM items i WHERE i.org_id=r.org_id AND i.run_id=r.id AND i.id=$5 AND i.state='streaming' AND i.revision=$6)").bind(lease.org_id).bind(run).bind(lease.epoch).bind(&progress.text).bind(Uuid::parse_str(&progress.item.id)?).bind(progress.item.revision as i64).execute(&mut *tx).await?.rows_affected()==1;
                tx.commit().await?;
                if valid {
                    let fold=ItemFold{item_id:progress.item.id.clone(),stream:"text".into(),part_index:0,text:progress.text.clone(),base_seq:progress.item.last_seq,revision:progress.item.revision};
                    live.insert(progress.item.id.clone(),fold);
                    let delta=Delta{item_id:progress.item.id,stream:"text".into(),part_index:0,offset:progress.offset as u64,text:progress.text[progress.offset..].into(),base_seq:progress.item.last_seq,revision:progress.item.revision};
                    owner::publish(&store,&nats,lease.org_id,&lease.channel(),Some(&lease),vec![frame(lease.channel(),Body::Delta(delta))]).await?;
                }
            }
            Some(request)=folds.next()=>{
                store.renew(&lease,config.owner_lease_ms).await?;
                if let Some(reply)=request.reply {
                    let batch=FrameBatch{frames:live.values().map(|fold|{let mut f=frame(lease.channel(),Body::ItemFold(fold.clone()));f.writer_epoch=lease.epoch as u64;f}).collect()};
                    nats.publish(reply,batch.encode_to_vec().into()).await?;
                }
            }
        }
    }
}
pub async fn lock(tx: &mut Tx<'_>, lease: &Lease) -> anyhow::Result<()> {
    let valid:Option<String>=sqlx::query_scalar("SELECT id FROM channels WHERE org_id=$1 AND id=$2 AND owner_id=$3 AND writer_epoch=$4 AND lease_until>clock_timestamp() FOR UPDATE").bind(lease.org_id).bind(lease.channel()).bind(lease.owner_id).bind(lease.epoch).fetch_optional(&mut **tx).await?;
    ensure!(valid.is_some(), "FENCED");
    Ok(())
}
pub async fn guard(tx: &mut Tx<'_>, lease: &Lease, run: Uuid) -> anyhow::Result<()> {
    lock(tx, lease).await?;
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs r JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id JOIN memberships m ON m.org_id=c.org_id AND m.user_id=c.creator_id WHERE r.org_id=$1 AND r.id=$2 AND r.exec_epoch=$3 AND r.state='running')").bind(lease.org_id).bind(run).bind(lease.epoch).fetch_one(&mut **tx).await?;
    ensure!(valid, "FENCED");
    Ok(())
}
pub async fn finish(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    state: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    let row=sqlx::query("UPDATE runs SET state=$4,ended_at=now(),error_code=$5 WHERE org_id=$1 AND id=$2 AND exec_epoch=$3 AND state IN ('running','requires_action') RETURNING assistant_item_id,response_text,read_only").bind(lease.org_id).bind(run).bind(lease.epoch).bind(state).bind(error).fetch_optional(&mut **tx).await?;
    let Some(row) = row else {
        return Ok(());
    };
    let pending: Vec<Uuid> = sqlx::query_scalar("SELECT item_id FROM pending_inputs WHERE org_id=$1 AND run_id=$2 AND kind='steer' AND consumed_step IS NULL").bind(lease.org_id).bind(run).fetch_all(&mut **tx).await?;
    for id in pending {
        let mut item = commands::item(tx, lease.org_id, id).await?;
        item.revision += 1;
        item.state = "cancelled".into();
        committed_item(tx, lease, item).await?;
    }
    let mut item = commands::item(tx, lease.org_id, row.get("assistant_item_id")).await?;
    if item.state == "streaming" || state != "completed" {
        item.revision += 1;
        item.state = state.into();
        item.text = row.get("response_text");
        if let Some(error) = error
            && state == "failed"
        {
            item.text.push_str(&format!("\n\n执行失败：{error}"));
        }
        append(
            tx,
            lease.org_id,
            &lease.channel(),
            Some(lease),
            "",
            Event::ItemUpserted(item),
        )
        .await?;
    }
    let actions:Vec<ActionRow>=sqlx::query_as("UPDATE pending_actions SET status='invalidated' WHERE org_id=$1 AND run_id=$2 AND status='pending' RETURNING id,run_id,run_epoch,summary,options,status,resolved_device,expires_at").bind(lease.org_id).bind(run).fetch_all(&mut **tx).await?;
    for a in actions {
        append(
            tx,
            lease.org_id,
            &lease.channel(),
            Some(lease),
            "",
            Event::PendingActionChanged(a.into()),
        )
        .await?;
    }
    sqlx::query("UPDATE virtual_keys SET revoked_at=now() WHERE org_id=$1 AND run_id=$2 AND revoked_at IS NULL").bind(lease.org_id).bind(run).execute(&mut **tx).await?;
    append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        "",
        Event::RunStateChanged(RunState {
            read_only: row.get("read_only"),
            run_id: run.to_string(),
            state: state.into(),
            exec_epoch: lease.epoch as u64,
        }),
    )
    .await?;
    Ok(())
}

pub async fn committed_item(tx: &mut Tx<'_>, lease: &Lease, item: Item) -> anyhow::Result<Item> {
    let event = append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        "",
        Event::ItemUpserted(item),
    )
    .await?;
    match event.event.context("missing item event")? {
        Event::ItemUpserted(item) => Ok(item),
        _ => unreachable!(),
    }
}
