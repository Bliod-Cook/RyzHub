//! The connection where WebSockets do not get through: frames to the client as server-sent
//! events, frames from it as HTTP posts.
//!
//! `GET /stream?ticket=&device_id=` opens the connection (authenticated like `/ws`). Its first
//! event, `connection`, names it; every later event's data is a frame, protobuf in base64.
//! The client posts its frames, one per request, to `POST /stream/{connection}` with its access
//! token. A connection lives in the process that opened it: behind several gateways, posts must
//! reach the same one (sticky sessions).
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
};
use base64::Engine as _;
use futures_util::{Stream, StreamExt};
use prost::Message as _;
use ryzhub_proto::{Frame, Hello, PROTOCOL_MAJOR};
use serde::Deserialize;
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use super::{Inbound, Outbound, SEND_QUEUE, serve, upgrade_auth};
use crate::{
    app::App,
    error::{ApiError, Code},
    http::authenticate,
};

/// Frames a client may post ahead of the connection taking them.
const POSTED: usize = 64;

/// An open stream connection: its user, and where frames posted to it go.
type Connection = (Uuid, mpsc::Sender<Frame>);

/// The open stream connections of this process, by id.
#[derive(Clone, Default)]
pub struct Streams(Arc<Mutex<HashMap<Uuid, Connection>>>);

#[derive(Deserialize)]
pub struct StreamQuery {
    ticket: Option<String>,
    #[serde(default)]
    device_id: String,
}

/// Frames to the client, as events.
struct EventsOut(mpsc::Sender<Event>);

impl Outbound for EventsOut {
    async fn send(&mut self, frame: &Frame) -> anyhow::Result<()> {
        let data = base64::engine::general_purpose::STANDARD.encode(frame.encode_to_vec());
        self.0.send(Event::default().data(data)).await?;
        Ok(())
    }

    async fn close(&mut self) {}
}

/// `GET /stream`
pub async fn open(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<StreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let auth = upgrade_auth(&app, &headers, query.ticket.as_deref()).await?;
    require!(
        !query.device_id.is_empty() && query.device_id.len() <= 128,
        Code::InvalidInput
    );
    // Unguessable: with the user's token, it is what posting needs.
    let id = uuid::Builder::from_random_bytes(rand::random()).into_uuid();
    let (posted, inbound) = mpsc::channel(POSTED);
    let (events, outbound) = mpsc::channel(SEND_QUEUE);
    app.streams
        .0
        .lock()
        .unwrap()
        .insert(id, (auth.user_id, posted));
    events
        .send(Event::default().event("connection").data(id.to_string()))
        .await
        .map_err(|_| Code::TemporarilyUnavailable)?;
    let hello = Hello {
        protocol_major: PROTOCOL_MAJOR,
        device_id: query.device_id,
        ..Default::default()
    };
    tokio::spawn(async move {
        let inbound = ReceiverStream::new(inbound).map(|f| Ok(Inbound::Frame(Box::new(f))));
        if let Err(error) = serve(app.clone(), hello, auth, inbound, EventsOut(events), id).await {
            tracing::debug!(%error, "event stream closed");
        }
        app.streams.0.lock().unwrap().remove(&id);
    });
    Ok(Sse::new(ReceiverStream::new(outbound).map(Ok))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

/// `POST /stream/{connection}`: one frame, protobuf, from the client.
pub async fn post(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let auth = authenticate(&app, &headers).await?;
    let connection = app.streams.0.lock().unwrap().get(&id).cloned();
    let Some((user, posted)) = connection.filter(|(user, _)| *user == auth.user_id) else {
        return Err(Code::UnknownStream.into());
    };
    debug_assert_eq!(user, auth.user_id);
    let frame = Frame::decode(body).map_err(|_| Code::InvalidCommand)?;
    tokio::time::timeout(Duration::from_secs(5), posted.send(frame))
        .await
        .map_err(|_| Code::Busy)?
        .map_err(|_| Code::UnknownStream)?;
    Ok(StatusCode::NO_CONTENT)
}
