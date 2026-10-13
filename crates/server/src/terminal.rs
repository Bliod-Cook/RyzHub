//! Work terminals: one WebSocket per terminal session, relayed to the sandbox's execution node.
//!
//! `GET /orgs/{org}/conversations/{conv}/terminal?session=&cols=&rows=&since=` upgrades to a
//! WebSocket (authenticated like `/ws`). Binary messages carry terminal bytes both ways. Text
//! messages carry control: from the client `{"resize":{"cols":c,"rows":r}}` and
//! `{"close":true}` (end the session); from the server `{"attached":{"start":n}}` once
//! connected and `{"exit":code}` when the shell exits.
//!
//! A client that leaves without `close` detaches: the shell keeps running, and the client
//! resumes by reconnecting with `since`, the count of output bytes it has. Terminal bytes never
//! enter the event stream; the connection is the session's own, end to end.
use anyhow::Context;
use axum::{
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::HeaderMap,
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use ryzhub_execution::{
    HostCommand,
    pty::{Frame, read_frame, write_frame},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

use crate::{
    app::App,
    error::{ApiError, Code},
    store::conversation_in,
};

/// How often an attached terminal marks its sandbox as in use.
const SEEN_EVERY: Duration = Duration::from_secs(30);
/// Largest terminal input message.
const MAX_INPUT: usize = 64 * 1024;

#[derive(Deserialize)]
pub struct TerminalQuery {
    ticket: Option<String>,
    session: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    since: Option<u64>,
}

pub async fn upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
    Query(query): Query<TerminalQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let auth = crate::ws::upgrade_auth(&app, &headers, query.ticket.as_deref()).await?;
    let mut tx = app.store.tx(org).await?;
    let ctx = crate::store::context_in(&mut tx, auth.user_id, org).await?;
    let conversation = conversation_in(&mut tx, &ctx, conv).await?;
    require!(conversation.mode == "work", Code::WorkOnly);
    let sandbox: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT id,exec_epoch FROM sandboxes WHERE org_id=$1 AND conversation_id=$2
         AND persistent AND state='running'",
    )
    .bind(org)
    .bind(conv)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let (sandbox, epoch) = sandbox.context(Code::WorkspaceNotRunning)?;
    let command = HostCommand::TerminalAttach {
        sandbox_id: sandbox,
        exec_epoch: epoch as u64,
        session: query.session.unwrap_or_else(|| "default".into()),
        cols: query.cols.unwrap_or(80).clamp(10, 500),
        rows: query.rows.unwrap_or(24).clamp(2, 300),
        since: query.since.unwrap_or(0),
    };
    let socket = crate::sandbox::endpoint(&app, org, sandbox).await?;
    let (reply, host_read, host_write) = ryzhub_execution::attach(&socket, &command).await?;
    Ok(ws
        .max_message_size(MAX_INPUT)
        .on_upgrade(move |client| async move {
            if let Err(error) =
                relay(&app, org, sandbox, client, reply, host_read, host_write).await
            {
                tracing::debug!(%error, "terminal closed");
            }
        }))
}

async fn relay(
    app: &App,
    org: Uuid,
    sandbox: Uuid,
    client: WebSocket,
    reply: Value,
    mut host_read: impl tokio::io::AsyncRead + Unpin,
    mut host_write: impl tokio::io::AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    let (mut to_client, mut from_client) = client.split();
    let attached = json!({"attached": {"start": reply["start"]}});
    to_client
        .send(Message::Text(attached.to_string().into()))
        .await?;
    let mut seen = tokio::time::interval(SEEN_EVERY);
    loop {
        tokio::select! {
            message = from_client.next() => {
                let frame = match message {
                    Some(Ok(Message::Binary(bytes))) => Frame::Data(bytes.to_vec()),
                    Some(Ok(Message::Text(text))) => {
                        let control: Value = serde_json::from_str(&text)?;
                        if control["close"] == true {
                            Frame::Close
                        } else if let (Some(cols), Some(rows)) = (
                            control["resize"]["cols"].as_u64(),
                            control["resize"]["rows"].as_u64(),
                        ) {
                            Frame::Resize { cols: cols.clamp(10, 500) as u16, rows: rows.clamp(2, 300) as u16 }
                        } else {
                            continue;
                        }
                    }
                    Some(Ok(_)) => continue,
                    // The client left: the session stays, detached.
                    None | Some(Err(_)) => return Ok(()),
                };
                let close = frame == Frame::Close;
                write_frame(&mut host_write, &frame).await?;
                if close {
                    return Ok(());
                }
            }
            frame = read_frame(&mut host_read) => {
                match frame? {
                    Some(Frame::Data(bytes)) => to_client.send(Message::Binary(bytes.into())).await?,
                    Some(Frame::Exit(code)) => {
                        to_client.send(Message::Text(json!({"exit": code}).to_string().into())).await?;
                        let _ = to_client.close().await;
                        return Ok(());
                    }
                    Some(_) => {}
                    // The node cut the connection: the sandbox stopped or was fenced.
                    None => {
                        let _ = to_client.close().await;
                        return Ok(());
                    }
                }
            }
            _ = seen.tick() => {
                let mut tx = app.store.tx(org).await?;
                sqlx::query("UPDATE sandboxes SET terminal_seen_at=now() WHERE org_id=$1 AND id=$2")
                    .bind(org)
                    .bind(sandbox)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
            }
        }
    }
}
