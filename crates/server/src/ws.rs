use crate::{
    api::{ApiError, App, authenticate},
    owner::{fold_subject, subject},
    store::{Auth, hash_token, new_token},
};
use anyhow::{Context, ensure};
use axum::{
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::HeaderMap,
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use ryzhub_proto::{frame::Body, *};
use serde::Deserialize;
use std::{collections::HashMap, time::Duration};
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
};
use uuid::Uuid;

#[derive(Deserialize)]
pub struct WsQuery {
    ticket: Option<String>,
}
pub async fn upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if let Some(origin) = headers.get("origin") {
        crate::api::check(
            app.config
                .allowed_origins
                .iter()
                .any(|o| origin.as_bytes() == o.as_bytes()),
            "FORBIDDEN",
        )?;
    }
    let auth = if let Some(ticket) = query.ticket {
        let session: Option<Uuid> = sqlx::query_scalar(
            "DELETE FROM ws_tickets WHERE token_hash=$1 AND expires_at>now() RETURNING session_id",
        )
        .bind(hash_token(&ticket))
        .fetch_optional(&app.store.pool)
        .await?;
        app.store
            .auth_by_session(session.context("UNAUTHORIZED")?)
            .await?
    } else {
        authenticate(&app, &headers).await?
    };
    Ok(ws
        .protocols([SUBPROTOCOL])
        .max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| async move {
            if let Err(error) = connection(app, socket, auth).await {
                tracing::debug!(%error,"websocket closed");
            }
        }))
}
struct Tasks(HashMap<String, JoinHandle<()>>);
impl Drop for Tasks {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.abort();
        }
    }
}
async fn connection(app: App, mut socket: WebSocket, mut auth: Auth) -> anyhow::Result<()> {
    let message = tokio::time::timeout(Duration::from_secs(10), socket.recv())
        .await?
        .context("closed")??;
    let Message::Binary(bytes) = message else {
        anyhow::bail!("binary Hello required")
    };
    let hello = Frame::decode(bytes)?.body.context("Hello required")?;
    let Body::Hello(hello) = hello else {
        anyhow::bail!("Hello required")
    };
    if hello.protocol_major != PROTOCOL_MAJOR
        || hello.device_id.is_empty()
        || hello.device_id.len() > 128
    {
        socket
            .send(Message::Binary(
                frame(
                    "",
                    Body::ProtocolError(ProtocolError {
                        code: "INCOMPATIBLE_PROTOCOL".into(),
                        message: "protocol major 1 and device_id required".into(),
                    }),
                )
                .encode_to_vec()
                .into(),
            ))
            .await?;
        return Ok(());
    }
    let restored:Option<serde_json::Value>=sqlx::query_scalar("DELETE FROM ws_resumes WHERE token_hash=$1 AND session_id=$2 AND device_id=$3 AND expires_at>now() RETURNING subscriptions").bind(hash_token(&hello.resume_token)).bind(auth.session_id).bind(&hello.device_id).fetch_optional(&app.store.pool).await?;
    let resumed = restored.is_some();
    let mut subscriptions: HashMap<String, Option<u64>> =
        serde_json::from_value(restored.unwrap_or_else(|| serde_json::json!({})))?;
    let resume_token = new_token();
    let resume_hash = hash_token(&resume_token);
    sqlx::query("DELETE FROM ws_resumes WHERE expires_at<now()")
        .execute(&app.store.pool)
        .await?;
    sqlx::query("INSERT INTO ws_resumes(token_hash,session_id,device_id,subscriptions,expires_at) VALUES($1,$2,$3,$4,now()+interval '3 minutes')").bind(&resume_hash).bind(auth.session_id).bind(&hello.device_id).bind(serde_json::to_value(&subscriptions)?).execute(&app.store.pool).await?;
    socket
        .send(Message::Binary(
            frame(
                "",
                Body::Welcome(Welcome {
                    connection_id: Uuid::now_v7().to_string(),
                    resume_token,
                    heartbeat_ms: 22_000,
                    max_frame_bytes: 1024 * 1024,
                    resumed,
                }),
            )
            .encode_to_vec()
            .into(),
        ))
        .await?;
    let (mut sink, mut source) = socket.split();
    let (send, mut receive) = mpsc::channel::<Frame>(256);
    let mut tasks = Tasks(HashMap::new());
    let mut commands = JoinSet::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(22));
    let mut auth_timer = tokio::time::interval(Duration::from_secs(1));
    let mut revoked = app
        .nats
        .subscribe(format!("ryzhub.revoke.{}", auth.user_id))
        .await?;
    let mut last_seen = tokio::time::Instant::now();
    for (channel, since) in subscriptions.clone() {
        spawn_subscription(
            &mut tasks,
            app.clone(),
            auth.clone(),
            channel,
            since,
            send.clone(),
        );
    }
    loop {
        tokio::select! {
            message=source.next()=>{
                let Some(message)=message else {break};
                let bytes=match message? {Message::Binary(bytes)=>bytes,Message::Close(_)=>break,Message::Pong(_)=>{last_seen=tokio::time::Instant::now();continue;},_=>continue};
                last_seen=tokio::time::Instant::now();
                let input=Frame::decode(bytes)?;
                match input.body {
                    Some(Body::Subscribe(sub))=>{
                        if let Some(old)=tasks.0.remove(&input.channel){old.abort();}
                        if tasks.0.len()>=16 {send.send(frame(&input.channel,Body::ChannelLagged(ChannelLagged {reason:"subscription limit".into()}))).await?;continue;}
                        subscriptions.insert(input.channel.clone(),sub.since_seq);
                        save_resume(&app,&resume_hash,&subscriptions).await?;
                        spawn_subscription(&mut tasks,app.clone(),auth.clone(),input.channel,sub.since_seq,send.clone());
                    }
                    Some(Body::Unsubscribe(_))=>{
                        if let Some(old)=tasks.0.remove(&input.channel){old.abort();}
                        subscriptions.remove(&input.channel);save_resume(&app,&resume_hash,&subscriptions).await?;
                    }
                    Some(Body::Command(command))=>{
                        if commands.len()>=32 {send.send(frame(input.channel,Body::CommandRejected(CommandRejected {cid:command.cid,code:"BUSY".into(),retryable:true}))).await?;continue;}
                        let app=app.clone();let auth=auth.clone();let send=send.clone();let device=hello.device_id.clone();
                        commands.spawn(async move {
                            let response=match handle_command(&app,&auth,&input.channel,&device,&command).await {
                                Ok(response)=>response,
                                Err(error)=>frame(&input.channel,Body::CommandRejected(CommandRejected {cid:command.cid,code:if matches!(error.to_string().as_str(),"FORBIDDEN"|"CID_REUSED"|"INVALID_CID"|"INVALID_COMMAND"|"UNAUTHORIZED") {error.to_string()}else{"TEMPORARILY_UNAVAILABLE".into()},retryable:!matches!(error.to_string().as_str(),"FORBIDDEN"|"CID_REUSED"|"INVALID_CID"|"INVALID_COMMAND"|"UNAUTHORIZED")})),
                            };
                            let _=send.send(response).await;
                        });
                    }
                    Some(Body::AuthRefresh(refresh))=>{
                        let refreshed=app.store.authenticate(&refresh.token).await?;
                        ensure!(refreshed.user_id==auth.user_id,"identity change requires reconnect");auth=refreshed;
                        // Recreate subscriptions on refresh so their session checks use the new session.
                        for (channel,task) in tasks.0.drain(){task.abort();send.send(frame(channel,Body::ChannelLagged(ChannelLagged {reason:"authentication refreshed".into()}))).await?;}
                    }
                    Some(Body::Pong(_))=>{}
                    _=>{send.send(frame("",Body::ProtocolError(ProtocolError {code:"UNEXPECTED_FRAME".into(),message:"unexpected client frame".into()}))).await?;}
                }
            }
            Some(frame)=receive.recv()=>{
                tokio::time::timeout(Duration::from_secs(5),sink.send(Message::Binary(frame.encode_to_vec().into()))).await??;
            }
            _=heartbeat.tick()=>{
                save_resume(&app,&resume_hash,&subscriptions).await?;
                ensure!(last_seen.elapsed()<Duration::from_secs(55),"heartbeat timeout");
                sink.send(Message::Binary(frame("",Body::Ping(Ping {nonce:chrono::Utc::now().timestamp_millis() as u64})).encode_to_vec().into())).await?;
            }
            _=auth_timer.tick()=>{
                if app.store.auth_by_session(auth.session_id).await.is_err() {
                    for channel in tasks.0.keys(){let _=sink.send(Message::Binary(frame(channel,Body::SubscriptionRevoked(SubscriptionRevoked {reason:"permission revoked".into()})).encode_to_vec().into())).await;}
                    break;
                }
                if (auth.expires_at-chrono::Utc::now()).num_seconds()<60 {sink.send(Message::Binary(frame("",Body::AuthExpiring(AuthExpiring {expires_at_ms:auth.expires_at.timestamp_millis()})).encode_to_vec().into())).await?;}
            }
            Some(_)=revoked.next()=>{
                if app.store.auth_by_session(auth.session_id).await.is_err() {
                    for channel in tasks.0.keys(){let _=sink.send(Message::Binary(frame(channel,Body::SubscriptionRevoked(SubscriptionRevoked {reason:"permission revoked".into()})).encode_to_vec().into())).await;}
                    break;
                }
            }
            Some(_)=commands.join_next(),if !commands.is_empty()=>{}
        }
    }
    let _ = sink.close().await;
    Ok(())
}
async fn handle_command(
    app: &App,
    auth: &Auth,
    channel: &str,
    device: &str,
    cmd: &CommandEnvelope,
) -> anyhow::Result<Frame> {
    app.store.auth_by_session(auth.session_id).await?;
    let ctx = app.store.authorize_channel(auth.user_id, channel).await?;
    let conv = Uuid::parse_str(channel.strip_prefix("conv:").context("INVALID_COMMAND")?)?;
    if let Some(result) = app.store.accept(&ctx, conv, device, cmd).await? {
        return Ok(result);
    }
    for _ in 0..100 {
        if let Some(result) = app
            .store
            .command_result(&ctx, Uuid::parse_str(&cmd.cid)?)
            .await?
        {
            return Ok(result);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("OWNER_UNAVAILABLE")
}
async fn deliver(send: &mpsc::Sender<Frame>, frame: Frame) -> anyhow::Result<()> {
    match send.try_send(frame) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(f)) => {
            tokio::time::timeout(
                Duration::from_secs(5),
                send.send(ryzhub_proto::frame(
                    f.channel,
                    Body::ChannelLagged(ChannelLagged {
                        reason: "send queue exceeded".into(),
                    }),
                )),
            )
            .await??;
            anyhow::bail!("LAGGED")
        }
        Err(_) => anyhow::bail!("CLOSED"),
    }
}
async fn subscription(
    app: &App,
    auth: &Auth,
    channel: &str,
    since: Option<u64>,
    send: &mpsc::Sender<Frame>,
) -> anyhow::Result<()> {
    let ctx = app.store.authorize_channel(auth.user_id, channel).await?;
    // Subscribe and flush first. The NATS subscriber buffers while the RR snapshot is read.
    let mut live = app.nats.subscribe(subject(channel)).await?;
    app.nats.flush().await?;
    let (frames, mut cursor) = app.store.replay(&ctx, channel, since).await?;
    for frame in frames {
        deliver(send, frame).await?;
    }
    if channel.starts_with("conv:")
        && let Ok(Ok(response)) = tokio::time::timeout(
            Duration::from_millis(500),
            app.nats.request(fold_subject(channel), "".into()),
        )
        .await
    {
        for fold in FrameBatch::decode(response.payload)?.frames {
            if current_publisher(app, &ctx, channel, fold.writer_epoch).await? {
                deliver(send, fold).await?;
            }
        }
    }
    deliver(
        send,
        frame(
            channel,
            Body::SubscriptionSynced(SubscriptionSynced { head_seq: cursor }),
        ),
    )
    .await?;
    let mut audit = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            Some(message)=live.next()=>{
                app.store.auth_by_session(auth.session_id).await?;
                app.store.authorize_channel(auth.user_id,channel).await?;
                let received=Frame::decode(message.payload)?;
            if !current_publisher(app,&ctx,channel,received.writer_epoch).await? {continue;}
                if let Some(Body::Event(event))=&received.body {
                    if event.seq<=cursor {continue;}
                    if event.seq>cursor+1 {
                        let (frames,head)=app.store.replay(&ctx,channel,Some(cursor)).await?;
                        for frame in frames {deliver(send,frame).await?;}cursor=head;
                        continue;
                    }
                    cursor=event.seq;
                }
                deliver(send,received).await?;
            }
            _=audit.tick()=>{
                app.store.auth_by_session(auth.session_id).await?;
                app.store.authorize_channel(auth.user_id,channel).await?;
                // Anti-entropy also repairs a lost final NATS message (no later seq to reveal a gap).
                let (frames,head)=app.store.replay(&ctx,channel,Some(cursor)).await?;
                for frame in frames {deliver(send,frame).await?;}cursor=head;
            }
            else=>return Ok(()),
        }
    }
}

async fn save_resume(
    app: &App,
    hash: &[u8],
    subscriptions: &HashMap<String, Option<u64>>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE ws_resumes SET subscriptions=$2,expires_at=now()+interval '3 minutes' WHERE token_hash=$1").bind(hash).bind(serde_json::to_value(subscriptions)?).execute(&app.store.pool).await?;
    Ok(())
}
fn spawn_subscription(
    tasks: &mut Tasks,
    app: App,
    auth: Auth,
    channel: String,
    since: Option<u64>,
    send: mpsc::Sender<Frame>,
) {
    tasks.0.insert(
        channel.clone(),
        tokio::spawn(async move {
            if let Err(error) = subscription(&app, &auth, &channel, since, &send).await
                && error.to_string() != "LAGGED"
            {
                let body = if matches!(
                    error.to_string().as_str(),
                    "FORBIDDEN" | "UNAUTHORIZED" | "access denied"
                ) {
                    Body::SubscriptionRevoked(SubscriptionRevoked {
                        reason: "permission revoked".into(),
                    })
                } else {
                    Body::ChannelLagged(ChannelLagged {
                        reason: "subscription unavailable; reconnect".into(),
                    })
                };
                let _ = send.send(frame(channel, body)).await;
            }
        }),
    );
}

async fn current_publisher(
    app: &App,
    ctx: &ryzhub_domain::OrgContext,
    channel: &str,
    epoch: u64,
) -> anyhow::Result<bool> {
    if !channel.starts_with("conv:") {
        return Ok(true);
    }
    let mut tx = app.store.tx(ctx.org_id).await?;
    let current: Option<i64> =
        sqlx::query_scalar("SELECT writer_epoch FROM channels WHERE org_id=$1 AND id=$2")
            .bind(ctx.org_id)
            .bind(channel)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(current == i64::try_from(epoch).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn bounded_queue_reports_lag_without_revoking_permission() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(frame("conv:test", Body::Ping(Ping { nonce: 1 })))
            .await
            .unwrap();
        let writer = tokio::spawn(async move {
            deliver(&tx, frame("conv:test", Body::Ping(Ping { nonce: 2 }))).await
        });
        tokio::task::yield_now().await;
        rx.recv().await.unwrap();
        assert!(matches!(
            rx.recv().await.unwrap().body,
            Some(Body::ChannelLagged(_))
        ));
        assert_eq!(writer.await.unwrap().unwrap_err().to_string(), "LAGGED");
    }
}
