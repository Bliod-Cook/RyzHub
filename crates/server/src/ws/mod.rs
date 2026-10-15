//! WebSocket gateway: connection handshake, subscriptions and command intake.
//!
//! Authorization is checked when a subscription starts, whenever a revocation notice may
//! affect it, and on a slow periodic fallback. Frames on the hot path are forwarded from the
//! process-wide [`Hub`] without database work.
//!
//! A connection is the same whatever carries it: a WebSocket speaking protobuf, or protobuf
//! with server frames through one zstd stream; or, where WebSockets do not get through,
//! server-sent events with frames posted back ([`sse`]).
mod hub;
pub mod sse;

pub use hub::{Feed, Hub};

use anyhow::{Context, ensure};
use axum::{
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::HeaderMap,
    response::Response,
};
use futures_util::{SinkExt, Stream, StreamExt, stream::SplitSink};
use prost::Message as _;
use ryzhub_proto::{frame::Body, *};
use serde::Deserialize;
use std::{collections::HashMap, time::Duration};
use tokio::{
    sync::{broadcast, mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::Instant,
};
use uuid::Uuid;

use crate::{
    app::App,
    error::{ApiError, Code, code_of},
    http::authenticate,
    owner::{fold_subject, result_subject, wake_subject},
    revocation::Notice,
    store::{Auth, hash_token},
    user_commands::Intake,
};

const MAX_SUBSCRIPTIONS: usize = 16;
const MAX_PENDING_COMMANDS: usize = 32;
pub(crate) const SEND_QUEUE: usize = 256;
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const HEARTBEAT: Duration = Duration::from_secs(22);
const CLIENT_SILENCE: Duration = Duration::from_secs(55);
/// Fallback re-authorization when no revocation notice arrives (e.g. listener reconnecting).
const REAUTHORIZE: Duration = Duration::from_secs(15);
/// How long a command waits for its Owner before the client is told to retry.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
pub struct WsQuery {
    ticket: Option<String>,
}

/// The session opening a WebSocket: an allowed Origin, and a one-time ticket (browsers) or a
/// bearer token.
pub async fn upgrade_auth(
    app: &App,
    headers: &HeaderMap,
    ticket: Option<&str>,
) -> Result<Auth, ApiError> {
    if let Some(origin) = headers.get("origin") {
        require!(
            app.config
                .allowed_origins
                .iter()
                .any(|allowed| origin.as_bytes() == allowed.as_bytes()),
            Code::Forbidden
        );
    }
    Ok(match ticket {
        Some(ticket) => {
            let session: Option<Uuid> = sqlx::query_scalar(
                "DELETE FROM ws_tickets WHERE token_hash=$1 AND expires_at>now() RETURNING session_id",
            )
            .bind(hash_token(ticket))
            .fetch_optional(&app.store.pool)
            .await?;
            app.store
                .auth_by_session(session.context(Code::Unauthorized)?)
                .await?
        }
        None => authenticate(app, headers).await?,
    })
}

pub async fn upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let auth = upgrade_auth(&app, &headers, query.ticket.as_deref()).await?;
    // The first protocol the client offers that this server speaks.
    Ok(ws
        .protocols([SUBPROTOCOL_ZSTD, SUBPROTOCOL])
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| async move {
            if let Err(error) = connection(app, socket, auth).await {
                tracing::debug!(%error, "websocket closed");
            }
        }))
}

/// How a WebSocket carries frames.
enum Codec {
    Protobuf,
    /// Frames to the client of at least [`ZSTD_FROM`] bytes as zstd frames, smaller ones as
    /// they are: a protobuf frame never starts with zstd's magic number, so the client tells
    /// them apart. Each message is a whole zstd frame (no context across messages: the pure
    /// Rust decoder clients run holds back a stream's last window until it ends); the large
    /// messages it shrinks (snapshots, history, long replies) carry nearly all the bytes.
    Zstd,
}

/// Frames smaller than this gain nothing from compression.
const ZSTD_FROM: usize = 256;
const ZSTD_LEVEL: i32 = 3;

impl Codec {
    fn negotiated(protocol: Option<&str>) -> Self {
        match protocol {
            Some(SUBPROTOCOL_ZSTD) => Codec::Zstd,
            _ => Codec::Protobuf,
        }
    }

    fn encode(&mut self, frame: &Frame) -> anyhow::Result<Message> {
        Ok(match self {
            Codec::Protobuf => Message::Binary(frame.encode_to_vec().into()),
            Codec::Zstd => {
                let bytes = frame.encode_to_vec();
                if bytes.len() < ZSTD_FROM {
                    return Ok(Message::Binary(bytes.into()));
                }
                let compressed = zstd::bulk::compress(&bytes, ZSTD_LEVEL)?;
                Message::Binary(if compressed.len() < bytes.len() {
                    compressed.into()
                } else {
                    bytes.into()
                })
            }
        })
    }
}

/// A message from the client: frames come as protobuf.
fn decode(message: Message) -> anyhow::Result<Inbound> {
    Ok(match message {
        Message::Binary(bytes) => Inbound::Frame(Box::new(Frame::decode(bytes)?)),
        Message::Close(_) => Inbound::Close,
        _ => Inbound::Activity,
    })
}

/// What arrives from the client.
pub enum Inbound {
    Frame(Box<Frame>),
    /// Something that shows the client is there, but carries no frame.
    Activity,
    Close,
}

/// Where frames to the client go.
pub trait Outbound: Send {
    fn send(&mut self, frame: &Frame) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn close(&mut self) -> impl Future<Output = ()> + Send;
}

struct WebSocketOut {
    sink: SplitSink<WebSocket, Message>,
    codec: Codec,
}

impl Outbound for WebSocketOut {
    async fn send(&mut self, frame: &Frame) -> anyhow::Result<()> {
        let message = self.codec.encode(frame)?;
        self.sink.send(message).await?;
        Ok(())
    }

    async fn close(&mut self) {
        let _ = self.sink.close().await;
    }
}

/// Subscription tasks keyed by channel; aborted when the connection ends.
struct Subscriptions(HashMap<String, JoinHandle<()>>);
impl Subscriptions {
    fn stop(&mut self, channel: &str) {
        if let Some(task) = self.0.remove(channel) {
            task.abort();
        }
    }
}

impl Drop for Subscriptions {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.abort();
        }
    }
}

/// The client's Hello, or None when it speaks another major version or names no device.
fn valid_hello(hello: Hello) -> Result<Hello, Box<Frame>> {
    if hello.protocol_major != PROTOCOL_MAJOR
        || hello.device_id.is_empty()
        || hello.device_id.len() > 128
    {
        let error = ProtocolError {
            code: Code::IncompatibleProtocol.to_string(),
            message: format!("protocol major {PROTOCOL_MAJOR} and device_id required"),
        };
        return Err(Box::new(frame("", Body::ProtocolError(error))));
    }
    Ok(hello)
}

async fn connection(app: App, socket: WebSocket, auth: Auth) -> anyhow::Result<()> {
    let protocol = socket
        .protocol()
        .and_then(|p| p.to_str().ok())
        .map(str::to_owned);
    let codec = Codec::negotiated(protocol.as_deref());
    let (sink, mut source) = socket.split();
    let mut out = WebSocketOut { sink, codec };
    let message = tokio::time::timeout(Duration::from_secs(10), source.next())
        .await?
        .context("closed before Hello")??;
    let Inbound::Frame(first) = decode(message)? else {
        anyhow::bail!("Hello required")
    };
    let Some(Body::Hello(hello)) = first.body else {
        anyhow::bail!("Hello required")
    };
    let hello = match valid_hello(hello) {
        Ok(hello) => hello,
        Err(refusal) => return out.send(&refusal).await,
    };
    let inbound = source.map(|message| decode(message?));
    serve(app, hello, auth, inbound, out, Uuid::now_v7()).await
}

/// A connection after its handshake: Welcome, then subscriptions and commands until the
/// client leaves or its session ends.
pub async fn serve(
    app: App,
    hello: Hello,
    auth: Auth,
    mut source: impl Stream<Item = anyhow::Result<Inbound>> + Unpin + Send,
    mut sink: impl Outbound,
    connection_id: Uuid,
) -> anyhow::Result<()> {
    // Clients resubscribe from their own persisted cursors, so no server-side resume exists.
    let welcome = Welcome {
        connection_id: connection_id.to_string(),
        ..Default::default()
    };
    // Online before welcomed, so nothing notifies a device that is already told.
    let user = auth.user_id;
    crate::push::online(&app, user, &hello.device_id, true).await;
    sink.send(&frame("", Body::Welcome(welcome))).await?;
    let (send, mut outgoing) = mpsc::channel::<Frame>(SEND_QUEUE);
    let (auth_tx, auth_rx) = watch::channel(auth);
    let mut subscriptions = Subscriptions(HashMap::new());
    let mut commands = JoinSet::new();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    let mut session_check = tokio::time::interval(REAUTHORIZE);
    let mut notices = app.revocations.subscribe();
    let mut last_seen = Instant::now();
    let expiring = tokio::time::sleep_until(expiry_warning(&auth_rx.borrow()));
    tokio::pin!(expiring);
    loop {
        tokio::select! {
            message = source.next() => {
                let Some(message) = message else { break };
                let input = match message? {
                    Inbound::Frame(frame) => *frame,
                    Inbound::Close => break,
                    Inbound::Activity => { last_seen = Instant::now(); continue; }
                };
                last_seen = Instant::now();
                match input.body {
                    Some(Body::Subscribe(subscribe)) => {
                        subscriptions.stop(&input.channel);
                        if subscriptions.0.len() >= MAX_SUBSCRIPTIONS {
                            let lagged = ChannelLagged { reason: "subscription limit".into() };
                            send.send(frame(&input.channel, Body::ChannelLagged(lagged))).await?;
                            continue;
                        }
                        let task = tokio::spawn(run_subscription(
                            app.clone(),
                            auth_rx.clone(),
                            input.channel.clone(),
                            subscribe.since_seq,
                            send.clone(),
                        ));
                        subscriptions.0.insert(input.channel, task);
                    }
                    Some(Body::Unsubscribe(_)) => subscriptions.stop(&input.channel),
                    Some(Body::Command(command)) => {
                        if commands.len() >= MAX_PENDING_COMMANDS {
                            send.send(rejection(&input.channel, &command.cid, Code::Busy)).await?;
                            continue;
                        }
                        let (app, auth, send, device) =
                            (app.clone(), auth_rx.borrow().clone(), send.clone(), hello.device_id.clone());
                        commands.spawn(async move {
                            let response = match handle_command(&app, &auth, &input.channel, &device, &command).await {
                                Ok(response) => response,
                                Err(error) => rejection(&input.channel, &command.cid, command_error(&error)),
                            };
                            let _ = send.send(response).await;
                        });
                    }
                    Some(Body::AuthRefresh(refresh)) => {
                        let refreshed = app.store.authenticate(&refresh.token).await?;
                        ensure!(refreshed.user_id == auth_rx.borrow().user_id, "identity change requires reconnect");
                        expiring.as_mut().reset(expiry_warning(&refreshed));
                        // Subscriptions re-authorize against the new session in place.
                        auth_tx.send_replace(refreshed);
                    }
                    // A Hello again (the stream transport posts the one it opened with) is no
                    // news.
                    Some(Body::Pong(_) | Body::Hello(_)) => {}
                    _ => {
                        let error = ProtocolError {
                            code: Code::UnexpectedFrame.to_string(),
                            message: "unexpected client frame".into(),
                        };
                        send.send(frame("", Body::ProtocolError(error))).await?;
                    }
                }
            }
            Some(frame) = outgoing.recv() => {
                tokio::time::timeout(Duration::from_secs(5), sink.send(&frame)).await??;
            }
            _ = heartbeat.tick() => {
                ensure!(last_seen.elapsed() < CLIENT_SILENCE, "heartbeat timeout");
                crate::push::online(&app, user, &hello.device_id, true).await;
                let ping = Ping { nonce: chrono::Utc::now().timestamp_millis() as u64 };
                sink.send(&frame("", Body::Ping(ping))).await?;
            }
            notice = notices.recv() => {
                let user = auth_rx.borrow().user_id;
                if affects_session(&notice, user) && !session_valid(&app, &auth_rx).await {
                    break;
                }
            }
            _ = session_check.tick() => {
                if !session_valid(&app, &auth_rx).await { break; }
            }
            () = &mut expiring => {
                let expires_at_ms = auth_rx.borrow().expires_at.timestamp_millis();
                sink.send(&frame("", Body::AuthExpiring(AuthExpiring { expires_at_ms }))).await?;
                expiring.as_mut().reset(Instant::now() + Duration::from_secs(24 * 3600));
            }
            Some(_) = commands.join_next(), if !commands.is_empty() => {}
        }
    }
    crate::push::online(&app, user, &hello.device_id, false).await;
    // A revoked session loses every channel at once. One that merely ran out loses nothing:
    // the client renews it and reconnects with its channels and queued commands intact.
    if session_revoked(&app, &auth_rx).await {
        for channel in subscriptions.0.keys() {
            let revoked = SubscriptionRevoked {
                reason: "permission revoked".into(),
            };
            let _ = sink
                .send(&frame(channel, Body::SubscriptionRevoked(revoked)))
                .await;
        }
    }
    sink.close().await;
    Ok(())
}

fn expiry_warning(auth: &Auth) -> Instant {
    let remaining = (auth.expires_at - chrono::Utc::now() - chrono::Duration::seconds(60))
        .to_std()
        .unwrap_or_default();
    Instant::now() + remaining
}

async fn session_valid(app: &App, auth: &watch::Receiver<Auth>) -> bool {
    let session = auth.borrow().session_id;
    app.store.auth_by_session(session).await.is_ok()
}

/// Whether the session was taken away (signed out, revoked, or its user disabled), as opposed
/// to expiring. Unknown when the database cannot say, which revokes nothing.
async fn session_revoked(app: &App, auth: &watch::Receiver<Auth>) -> bool {
    let session = auth.borrow().session_id;
    app.store.session_revoked(session).await.unwrap_or(false)
}

fn affects_session(notice: &Result<Notice, broadcast::error::RecvError>, user: Uuid) -> bool {
    match notice {
        Ok(notice) => {
            matches!(notice.table.as_str(), "auth_sessions" | "users")
                && notice.user_id.is_none_or(|id| id == user)
        }
        // A lagged or reset listener may have dropped a revocation.
        Err(_) => true,
    }
}

fn rejection(channel: &str, cid: &str, code: Code) -> Frame {
    frame(
        channel,
        Body::CommandRejected(CommandRejected {
            cid: cid.into(),
            code: code.to_string(),
            retryable: code.retryable(),
        }),
    )
}

/// Client errors keep their code; anything else is a transient gateway failure.
fn command_error(error: &anyhow::Error) -> Code {
    match code_of(error) {
        Some(code) => code,
        None => {
            tracing::warn!(error = format!("{error:#}"), "command intake failed");
            Code::TemporarilyUnavailable
        }
    }
}

/// Queue the command, wake its Owner and wait for the pushed result. The database remains
/// the source of truth: a lost push falls back to reading the stored result.
async fn handle_command(
    app: &App,
    auth: &Auth,
    channel: &str,
    device: &str,
    command: &CommandEnvelope,
) -> anyhow::Result<Frame> {
    app.store.auth_by_session(auth.session_id).await?;
    let user = auth.user_id;
    // Membership is checked where the command is taken, in its transaction.
    let org = app.store.channel_org(user, channel).await?;
    let cid = Uuid::parse_str(&command.cid).context(Code::InvalidCid)?;
    let mut results = app.nats.subscribe(result_subject(user, cid)).await?;
    let conv = if channel.starts_with("user:") {
        match crate::user_commands::accept(app, (org, user), channel, device, command).await? {
            Intake::Done(result) => return Ok(*result),
            Intake::Queued(conv) => conv,
        }
    } else {
        let conv = channel
            .strip_prefix("conv:")
            .and_then(|id| Uuid::parse_str(id).ok())
            .context(Code::InvalidCommand)?;
        if let Some(result) = app.store.accept(org, user, conv, device, command).await? {
            return Ok(result);
        }
        conv
    };
    app.nats
        .publish(wake_subject(org, conv), Default::default())
        .await?;
    let deadline = tokio::time::sleep(COMMAND_TIMEOUT);
    tokio::pin!(deadline);
    let mut fallback = tokio::time::interval_at(
        Instant::now() + Duration::from_millis(500),
        Duration::from_millis(500),
    );
    loop {
        tokio::select! {
            Some(message) = results.next() => return Ok(Frame::decode(message.payload)?),
            _ = fallback.tick() => {
                if let Some(result) = app.store.command_result(org, user, cid).await? {
                    return Ok(result);
                }
            }
            () = &mut deadline => anyhow::bail!(Code::OwnerUnavailable),
        }
    }
}

/// Queue a frame without blocking the hub. A full queue ends the subscription with
/// ChannelLagged; the client resubscribes from its persisted cursor.
async fn deliver(send: &mpsc::Sender<Frame>, frame: Frame) -> anyhow::Result<()> {
    match send.try_send(frame) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(frame)) => {
            let lagged = ChannelLagged {
                reason: "send queue exceeded".into(),
            };
            tokio::time::timeout(
                Duration::from_secs(5),
                send.send(ryzhub_proto::frame(
                    frame.channel,
                    Body::ChannelLagged(lagged),
                )),
            )
            .await??;
            Err(Lagged.into())
        }
        Err(mpsc::error::TrySendError::Closed(_)) => Err(Lagged.into()),
    }
}

/// The subscription ended because the client fell behind; it has already been told.
#[derive(Debug, thiserror::Error)]
#[error("subscription lagged")]
struct Lagged;

async fn run_subscription(
    app: App,
    auth: watch::Receiver<Auth>,
    channel: String,
    since: Option<u64>,
    send: mpsc::Sender<Frame>,
) {
    let Err(error) = subscription(&app, auth, &channel, since, &send).await else {
        return;
    };
    if error.downcast_ref::<Lagged>().is_some() {
        return;
    }
    let body = if matches!(code_of(&error), Some(Code::Forbidden | Code::Unauthorized)) {
        Body::SubscriptionRevoked(SubscriptionRevoked {
            reason: "permission revoked".into(),
        })
    } else {
        tracing::debug!(channel, error = format!("{error:#}"), "subscription ended");
        Body::ChannelLagged(ChannelLagged {
            reason: "subscription unavailable; reconnect".into(),
        })
    };
    let _ = send.send(frame(channel, body)).await;
}

async fn subscription(
    app: &App,
    mut auth: watch::Receiver<Auth>,
    channel: &str,
    since: Option<u64>,
    send: &mpsc::Sender<Frame>,
) -> anyhow::Result<()> {
    let user = auth.borrow().user_id;
    let ctx = app.store.authorize_channel(user, channel).await?;
    // Subscribe to live frames before reading the snapshot; duplicates are dropped by seq.
    let feed = app.hub.feed(ctx.org_id, channel).await?;
    let mut live = feed.subscribe();
    let (frames, mut cursor) = app.store.replay(&ctx, channel, since).await?;
    for frame in frames {
        deliver(send, frame).await?;
    }
    if channel.starts_with("conv:") {
        for fold in current_folds(app, channel).await {
            if fold.writer_epoch >= feed.epoch() {
                deliver(send, fold).await?;
            }
        }
    }
    let synced = SubscriptionSynced { head_seq: cursor };
    deliver(send, frame(channel, Body::SubscriptionSynced(synced))).await?;
    let mut notices = app.revocations.subscribe();
    let mut reauthorize = tokio::time::interval_at(Instant::now() + REAUTHORIZE, REAUTHORIZE);
    loop {
        tokio::select! {
            received = live.recv() => match received {
                Ok(frame) => {
                    if let Some(Body::Event(event)) = &frame.body {
                        if event.seq <= cursor {
                            continue;
                        }
                        if event.seq > cursor + 1 {
                            let (frames, head) = app.store.replay(&ctx, channel, Some(cursor)).await?;
                            for frame in frames {
                                deliver(send, frame).await?;
                            }
                            cursor = head;
                            continue;
                        }
                        cursor = event.seq;
                    }
                    deliver(send, Frame::clone(&frame)).await?;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let lagged = ChannelLagged { reason: "subscriber fell behind".into() };
                    deliver(send, ryzhub_proto::frame(channel, Body::ChannelLagged(lagged))).await?;
                    return Err(Lagged.into());
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            notice = notices.recv() => {
                if affects_subscription(&notice, ctx.org_id, user) {
                    authorize(app, &auth, channel).await?;
                }
            }
            _ = reauthorize.tick() => authorize(app, &auth, channel).await?,
            changed = auth.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                authorize(app, &auth, channel).await?;
            }
        }
    }
}

async fn authorize(app: &App, auth: &watch::Receiver<Auth>, channel: &str) -> anyhow::Result<()> {
    let (session, user) = {
        let auth = auth.borrow();
        (auth.session_id, auth.user_id)
    };
    app.store.auth_by_session(session).await?;
    app.store.authorize_channel(user, channel).await?;
    Ok(())
}

fn affects_subscription(
    notice: &Result<Notice, broadcast::error::RecvError>,
    org: Uuid,
    user: Uuid,
) -> bool {
    match notice {
        Ok(notice) => {
            matches!(
                notice.table.as_str(),
                "memberships" | "users" | "auth_sessions" | "organizations" | "conversations"
            ) && notice.touches_org(org)
                && notice.user_id.is_none_or(|id| id == user)
        }
        Err(_) => true,
    }
}

/// In-progress items from the current Owner, if one is live.
async fn current_folds(app: &App, channel: &str) -> Vec<Frame> {
    let request = app.nats.request(fold_subject(channel), Default::default());
    match tokio::time::timeout(Duration::from_millis(500), request).await {
        Ok(Ok(response)) => FrameBatch::decode(response.payload)
            .map(|batch| batch.frames)
            .unwrap_or_default(),
        _ => vec![],
    }
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
        assert!(
            writer
                .await
                .unwrap()
                .unwrap_err()
                .downcast_ref::<Lagged>()
                .is_some()
        );
    }
}
