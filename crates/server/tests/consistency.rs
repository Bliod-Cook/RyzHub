//! Requires the local PostgreSQL 18 + NATS services: scripts/test-integration.sh.
mod common;
use anyhow::{Context, ensure};
use common::{Harness, id};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use ryzhub_core::Engine;
use ryzhub_domain::OrgContext;
use ryzhub_proto::{command_envelope::Command, event_envelope::Event, frame::Body, *};
use ryzhub_server::{
    app::App,
    config::{Config, ModelRoute},
    error::{Code, code_of},
    owner::{self, OwnerLink},
    store::{Lease, append, create_in},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::time::Duration;
use tokio::{net::TcpStream, process::Command as Process};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message as WsMessage, client::IntoClientRequest},
};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
impl Harness {
    /// The Chat engine, over the fixture model.
    async fn new() -> anyhow::Result<Self> {
        Self::start(|config| config.llm.enabled = true).await
    }
    async fn conversation(&self, user: &Value) -> anyhow::Result<Uuid> {
        let ctx = self.context(user).await?;
        Ok(self
            .create(&ctx, "一致性验收", "chat", json!({}), None)
            .await?
            .id)
    }
}

/// How many of the channel's committed events are not yet confirmed as published.
async fn unpublished(db: &PgPool, channel: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM events e JOIN channels c ON c.org_id=e.org_id AND c.id=e.channel_id
         WHERE e.channel_id=$1 AND e.seq>c.published_seq",
    )
    .bind(channel)
    .fetch_one(db)
    .await?)
}
fn message(cid: Uuid, text: &str) -> CommandEnvelope {
    CommandEnvelope {
        cid: cid.to_string(),
        command: Some(Command::SendMessage(SendMessage {
            text: text.into(),
            ..Default::default()
        })),
    }
}
async fn connect(base: &str, user: &Value, device: &str) -> anyhow::Result<Socket> {
    let mut request = format!("{}/ws", base.replace("http:", "ws:")).into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", user["token"].as_str().unwrap()).parse()?,
    );
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", SUBPROTOCOL.parse()?);
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await?;
    send(
        &mut socket,
        frame(
            "",
            Body::Hello(Hello {
                protocol_major: 1,
                device_id: device.into(),
                ..Default::default()
            }),
        ),
    )
    .await?;
    ensure!(
        matches!(next(&mut socket).await?.body, Some(Body::Welcome(_))),
        "welcome missing"
    );
    Ok(socket)
}
async fn send(socket: &mut Socket, f: Frame) -> anyhow::Result<()> {
    socket
        .send(WsMessage::Binary(f.encode_to_vec().into()))
        .await?;
    Ok(())
}
async fn next(socket: &mut Socket) -> anyhow::Result<Frame> {
    loop {
        let input = tokio::time::timeout(Duration::from_secs(20), socket.next())
            .await?
            .context("socket closed")??;
        if let WsMessage::Binary(bytes) = input {
            let f = Frame::decode(bytes)?;
            if let Some(Body::Ping(p)) = f.body {
                send(socket, frame("", Body::Pong(Pong { nonce: p.nonce }))).await?;
                continue;
            }
            return Ok(f);
        }
    }
}
async fn until(
    socket: &mut Socket,
    mut predicate: impl FnMut(&Frame) -> bool,
) -> anyhow::Result<Frame> {
    for _ in 0..500 {
        let f = next(socket).await?;
        if predicate(&f) {
            return Ok(f);
        }
    }
    anyhow::bail!("expected frame not seen")
}
/// The event a frame carries.
fn event_of(f: &Frame) -> Option<&Event> {
    match &f.body {
        Some(Body::Event(e)) => e.event.as_ref(),
        _ => None,
    }
}
async fn subscribe(socket: &mut Socket, channel: &str, since: Option<u64>) -> anyhow::Result<()> {
    send(
        socket,
        frame(channel, Body::Subscribe(Subscribe { since_seq: since })),
    )
    .await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn an_expired_session_keeps_its_channels_and_a_revoked_one_loses_them() -> anyhow::Result<()>
{
    let h = Harness::new().await?;
    // Read what a socket sends until it closes: whether it revoked anything.
    async fn revokes_before_closing(socket: &mut Socket) -> bool {
        let mut revoked = false;
        while let Ok(f) = next(socket).await {
            revoked |= matches!(f.body, Some(Body::SubscriptionRevoked(_)));
        }
        revoked
    }
    for (expire, label) in [(true, "expired"), (false, "revoked")] {
        let user = h.user().await?;
        let conv = h.conversation(&user).await?;
        let channel = format!("conv:{conv}");
        let mut socket = connect(&h.base, &user, label).await?;
        subscribe(&mut socket, &channel, None).await?;
        until(&mut socket, |f| matches!(f.body, Some(Body::Snapshot(_)))).await?;
        let change = if expire {
            "UPDATE auth_sessions SET expires_at=now()-interval '1 second' WHERE user_id=$1"
        } else {
            "UPDATE auth_sessions SET revoked_at=now() WHERE user_id=$1"
        };
        sqlx::query(change)
            .bind(id(&user, "user_id"))
            .execute(&h.db)
            .await?;
        // Either way the connection ends; only a revoked session takes the channels with it.
        assert_eq!(
            revokes_before_closing(&mut socket).await,
            !expire,
            "{label}"
        );
    }
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn database_idempotency_fencing_rls_and_transaction_outbox() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let other = h.user().await?;
    let conv = h.conversation(&user).await?;
    let ctx = h
        .app
        .store
        .context(id(&user, "user_id"), id(&user, "org_id"))
        .await?;
    let channel = format!("conv:{conv}");
    let cid = Uuid::now_v7();
    let cmd = message(cid, "atomic transaction");
    assert!(
        h.app
            .store
            .accept(ctx.org_id, ctx.user_id, conv, "device-a", &cmd)
            .await?
            .is_none()
    );
    assert!(
        h.app
            .store
            .accept(ctx.org_id, ctx.user_id, conv, "device-b", &cmd)
            .await?
            .is_none()
    );
    let old = h
        .app
        .store
        .claim(ctx.org_id, conv, Uuid::now_v7(), 3000)
        .await?
        .unwrap();
    assert!(owner::process_with_config(&h.app.store, &old, &h.app.config).await?);
    let first = h
        .app
        .store
        .command_result(ctx.org_id, ctx.user_id, cid)
        .await?
        .unwrap();
    assert_eq!(
        h.app
            .store
            .accept(ctx.org_id, ctx.user_id, conv, "device-b", &cmd)
            .await?
            .unwrap(),
        first
    );
    assert!(
        h.app
            .store
            .accept(
                ctx.org_id,
                ctx.user_id,
                conv,
                "device-a",
                &message(cid, "different payload")
            )
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM commands WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(count, 1);
    let mut tx = h.app.store.tx(id(&other, "org_id")).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM items WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(count, 0);
    let foreign = sqlx::query(
        "INSERT INTO runs(id,org_id,conversation_id,harness,model,exec_epoch,state,response_text,
           assistant_item_id,cause_cid)
         VALUES($1,$2,$3,'chat','fixture-model',1,'running','x',$4,$5)",
    )
    .bind(Uuid::now_v7())
    .bind(id(&other, "org_id"))
    .bind(conv)
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(&mut *tx)
    .await;
    assert!(foreign.is_err());
    tx.rollback().await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM items")
            .fetch_one(&h.app.store.pool)
            .await?,
        0
    );
    assert!(
        h.app
            .store
            .authorize_channel(id(&other, "user_id"), &channel)
            .await
            .is_err()
    );
    assert_eq!(unpublished(&h.db, &channel).await?, 3);
    // Process crashes after commit and before publishing: force only its lease to expire.
    sqlx::query("UPDATE channels SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(&channel)
        .execute(&h.db)
        .await?;
    let new = h
        .app
        .store
        .claim(ctx.org_id, conv, Uuid::now_v7(), 3000)
        .await?
        .unwrap();
    assert!(new.epoch > old.epoch);
    assert!(h.app.store.renew(&old, 3000).await.is_err());
    let mut tx = h.app.store.tx(ctx.org_id).await?;
    assert!(
        append(
            &mut tx,
            ctx.org_id,
            &channel,
            Some(&old),
            "",
            Event::RunStateChanged(RunState::default())
        )
        .await
        .is_err()
    );
    tx.rollback().await?;
    assert!(
        owner::publish(
            &h.app.store,
            &h.app.nats,
            ctx.org_id,
            &channel,
            Some(&old),
            vec![]
        )
        .await
        .is_err()
    );
    let mut subscriber = h.app.nats.subscribe(owner::subject(&channel)).await?;
    h.app.nats.flush().await?;
    owner::publish(
        &h.app.store,
        &h.app.nats,
        ctx.org_id,
        &channel,
        Some(&new),
        vec![],
    )
    .await?;
    let mut engine = Engine::default();
    engine.subscribe(channel.clone());
    for expected in 1..=4 {
        let msg = tokio::time::timeout(Duration::from_secs(3), subscriber.next())
            .await?
            .unwrap();
        let f = Frame::decode(msg.payload)?;
        assert!(matches!(&f.body,Some(Body::Event(e)) if e.seq==expected));
        engine.apply(f);
    }
    assert_eq!(engine.channels[&channel].cursor, 4);
    assert_eq!(unpublished(&h.db, &channel).await?, 0);
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn disabled_engines_refuse_work_conversations_and_runs() -> anyhow::Result<()> {
    // Work is off, its model still configured: an instance that turned Work off.
    let h = Harness::start(|config| {
        config.llm.enabled = true;
        config.work.model = "gpt-5.4".into();
    })
    .await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let ctx = h.context(&user).await?;
    let mut create = create_run(Uuid::now_v7(), Uuid::now_v7(), "hello");
    if let Some(Command::CreateRun(c)) = &mut create.command {
        c.mode = "work".into();
    }
    let channel = format!("user:{org}:{}", ctx.user_id);
    let refused =
        ryzhub_server::user_commands::accept(&h.app, (org, ctx.user_id), &channel, "a", &create)
            .await;
    assert_eq!(
        refused.err().and_then(|e| code_of(&e)),
        Some(Code::WorkDisabled)
    );
    // A Work conversation from before, and a Chat one where no model gateway runs.
    let mut without_models = (*h.app.config).clone();
    without_models.llm.enabled = false;
    let mut before = (*h.app.config).clone();
    before.work.enabled = true;
    for (mode, config, code) in [
        ("work", &*h.app.config, "WORK_DISABLED"),
        ("chat", &without_models, "MODEL_GATEWAY_DISABLED"),
    ] {
        let mut tx = h.app.store.tx(org).await?;
        let conv = create_in(
            &mut tx,
            &ctx,
            Uuid::now_v7(),
            "关闭",
            mode,
            json!({}),
            None,
            &before,
        )
        .await?
        .unwrap()
        .id;
        tx.commit().await?;
        let cid = Uuid::now_v7();
        h.app
            .store
            .accept(ctx.org_id, ctx.user_id, conv, "a", &message(cid, "hello"))
            .await?;
        let lease = h
            .app
            .store
            .claim(org, conv, Uuid::now_v7(), 10_000)
            .await?
            .unwrap();
        owner::process_with_config(&h.app.store, &lease, config).await?;
        let result = h
            .app
            .store
            .command_result(ctx.org_id, ctx.user_id, cid)
            .await?
            .unwrap();
        assert!(
            matches!(&result.body, Some(Body::CommandRejected(r)) if r.code == code),
            "{mode}: {result:?}"
        );
    }
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn two_devices_answer_cas_and_stale_execution_epoch() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let ctx = h
        .app
        .store
        .context(id(&user, "user_id"), id(&user, "org_id"))
        .await?;
    h.app
        .store
        .accept(
            ctx.org_id,
            ctx.user_id,
            conv,
            "a",
            &message(Uuid::now_v7(), "question"),
        )
        .await?;
    let lease = h
        .app
        .store
        .claim(ctx.org_id, conv, Uuid::now_v7(), 10_000)
        .await?
        .unwrap();
    owner::process_with_config(&h.app.store, &lease, &h.app.config).await?;
    let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    // The model asks; the run waits for the answer.
    ryzhub_server::chat::runner::drive(h.app.clone(), lease, run, OwnerLink::detached()).await?;
    let (action, epoch) = h.action(run).await?;
    let answer = |epoch: i64| CommandEnvelope {
        cid: Uuid::now_v7().to_string(),
        command: Some(Command::AnswerQuestion(AnswerQuestion {
            action_id: action.to_string(),
            run_epoch: epoch as u64,
            answer: "蓝色".into(),
        })),
    };
    // A takeover keeps the question visible but invalidates the old execution epoch.
    sqlx::query(
        "UPDATE channels SET lease_until=now()-interval '1 second' WHERE conversation_id=$1",
    )
    .bind(conv)
    .execute(&h.db)
    .await?;
    let lease = h
        .app
        .store
        .claim(ctx.org_id, conv, Uuid::now_v7(), 10_000)
        .await?
        .unwrap();
    let stale = answer(epoch);
    h.app
        .store
        .accept(ctx.org_id, ctx.user_id, conv, "old-device", &stale)
        .await?;
    owner::process_with_config(&h.app.store, &lease, &h.app.config).await?;
    assert!(matches!(
        h.app
            .store
            .command_result(ctx.org_id, ctx.user_id, Uuid::parse_str(&stale.cid)?)
            .await?
            .unwrap()
            .body,
        Some(Body::CommandRejected(_))
    ));
    let (a, b) = (answer(lease.epoch), answer(lease.epoch));
    let (ra, rb) = tokio::join!(
        h.app.store.accept(ctx.org_id, ctx.user_id, conv, "a", &a),
        h.app.store.accept(ctx.org_id, ctx.user_id, conv, "b", &b)
    );
    ra?;
    rb?;
    owner::process_with_config(&h.app.store, &lease, &h.app.config).await?;
    owner::process_with_config(&h.app.store, &lease, &h.app.config).await?;
    let mut accepted = 0;
    let mut rejected = 0;
    for cmd in [a, b] {
        match h
            .app
            .store
            .command_result(ctx.org_id, ctx.user_id, Uuid::parse_str(&cmd.cid)?)
            .await?
            .unwrap()
            .body
            .unwrap()
        {
            Body::CommandAck(_) => accepted += 1,
            Body::CommandRejected(r) => {
                assert_eq!(r.code, "ACTION_ALREADY_RESOLVED");
                rejected += 1;
            }
            _ => panic!("wrong result"),
        }
    }
    assert_eq!((accepted, rejected), (1, 1));
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn websocket_reconnect_owner_process_restart_snapshot_and_isolation() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let other = h.user().await?;
    let conv = h.conversation(&user).await?;
    let channel = format!("conv:{conv}");
    let mut owner = h.owner()?;
    let mut socket = connect(&h.base, &user, "desktop").await?;
    let mut engine = Engine::default();
    engine.subscribe(channel.clone());
    subscribe(&mut socket, &channel, None).await?;
    let cid = Uuid::now_v7();
    send(
        &mut socket,
        frame(
            &channel,
            Body::Command(message(cid, &"切网与接管 slow ".repeat(35))),
        ),
    )
    .await?;
    let mut ack = None;
    let mut saw_delta = false;
    while ack.is_none() || !saw_delta {
        let f = next(&mut socket).await?;
        if matches!(f.body, Some(Body::Delta(_))) {
            saw_delta = true;
        }
        if let Some(Body::CommandAck(a)) = &f.body {
            ack = Some(a.clone());
        }
        engine.apply(f);
    }
    // Deliberately forget the ACK and repeat exactly the same command.
    send(
        &mut socket,
        frame(
            &channel,
            Body::Command(message(cid, &"切网与接管 slow ".repeat(35))),
        ),
    )
    .await?;
    let duplicate = until(
        &mut socket,
        |f| matches!(&f.body,Some(Body::CommandAck(a)) if a.cid==cid.to_string()),
    )
    .await?;
    assert_eq!(duplicate.body, Some(Body::CommandAck(ack.unwrap())));
    let saved = serde_json::to_string(&engine)?;
    socket.close(None).await?;
    owner.kill().await?;
    owner.wait().await?;
    let mut owner = h.owner()?;
    let mut socket = connect(&h.base, &user, "desktop-new-network").await?;
    let mut engine: Engine = serde_json::from_str(&saved)?;
    subscribe(
        &mut socket,
        &channel,
        Some(engine.channels[&channel].cursor),
    )
    .await?;
    let mut completed = false;
    for _ in 0..400 {
        let f = next(&mut socket).await?;
        let effects = engine.apply(f);
        for f in effects.outbound {
            send(&mut socket, f).await?;
        }
        if engine.channels[&channel]
            .items
            .values()
            .any(|i| i.kind == "assistant" && i.state == "completed")
        {
            completed = true;
            break;
        }
    }
    assert!(completed);
    let final_text = engine.channels[&channel]
        .items
        .values()
        .find(|i| i.kind == "assistant")
        .unwrap()
        .text
        .clone();
    assert!(final_text.ends_with("你好 Rust"), "{final_text}");
    let mut intruder = connect(&h.base, &other, "intruder").await?;
    subscribe(&mut intruder, &channel, None).await?;
    assert!(matches!(
        next(&mut intruder).await?.body,
        Some(Body::SubscriptionRevoked(_))
    ));
    // Retention cleanup removes replay data but preserves authoritative items.
    sqlx::query("DELETE FROM events WHERE channel_id=$1")
        .bind(&channel)
        .execute(&h.db)
        .await?;
    let mut restored = connect(&h.base, &user, "phone").await?;
    subscribe(&mut restored, &channel, Some(0)).await?;
    let snapshot = until(&mut restored, |f| matches!(f.body, Some(Body::Snapshot(_)))).await?;
    let Some(Body::Snapshot(s)) = snapshot.body else {
        unreachable!()
    };
    assert!(s.items.iter().any(|i| i.text == final_text));
    assert!(s.head_seq > 0);
    // Browser tickets are one-use, session-bound, and never substitute for channel authorization.
    let ticket: Value = reqwest::Client::new()
        .post(format!("{}/auth/ticket", h.base))
        .bearer_auth(user["token"].as_str().unwrap())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let ticket_url = format!(
        "{}/ws?ticket={}",
        h.base.replace("http:", "ws:"),
        ticket["ticket"].as_str().unwrap()
    );
    let (mut ticket_socket, _) = tokio_tungstenite::connect_async(&ticket_url).await?;
    assert!(tokio_tungstenite::connect_async(&ticket_url).await.is_err());
    ticket_socket.close(None).await?;
    owner.kill().await?;
    owner.wait().await?;
    socket.close(None).await?;
    intruder.close(None).await?;
    restored.close(None).await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn the_standalone_gateway_process_serves_client_subscriptions() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let reserve = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = reserve.local_addr()?;
    drop(reserve);
    let mut config = tempfile::NamedTempFile::new()?;
    std::io::Write::write_all(&mut config, format!("listen = \"{addr}\"\n").as_bytes())?;
    let mut gateway = Process::new(env!("CARGO_BIN_EXE_ryzhub-server"))
        .arg("gateway")
        .env("DATABASE_URL", &h.app_url)
        .env("NATS_URL", "nats://127.0.0.1:54222")
        .env("RYZHUB_CONFIG", config.path())
        .kill_on_drop(true)
        .spawn()?;
    let base = format!("http://{addr}");
    let mut ready = false;
    for _ in 0..100 {
        if reqwest::get(format!("{base}/healthz")).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    ensure!(ready, "gateway startup timeout");
    let mut socket = connect(&base, &user, "gateway-client").await?;
    let channel = format!("conv:{conv}");
    subscribe(&mut socket, &channel, None).await?;
    let synced = until(&mut socket, |f| {
        matches!(f.body, Some(Body::SubscriptionSynced(_)))
    })
    .await?;
    assert_eq!(synced.channel, channel);
    gateway.kill().await?;
    gateway.wait().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn refresh_cookie_rotates_survives_concurrent_tabs_and_revokes_replays() -> anyhow::Result<()>
{
    let h = Harness::new().await?;
    let http = reqwest::Client::new();
    let cookie = |response: &reqwest::Response| {
        response
            .headers()
            .get("set-cookie")
            .map(|v| v.to_str().unwrap().to_owned())
    };
    let token = |set: &str| {
        set.split(';')
            .next()
            .unwrap()
            .strip_prefix("ryzhub_refresh=")
            .unwrap()
            .to_owned()
    };
    let refresh = |value: String| {
        http.post(format!("{}/auth/refresh", h.base))
            .header("cookie", format!("ryzhub_refresh={value}"))
            .send()
    };
    let me = |access: Value| {
        http.get(format!("{}/me", h.base))
            .bearer_auth(access["token"].as_str().unwrap().to_owned())
            .send()
    };
    let email = format!("{}@example.test", Uuid::now_v7());
    let registered = http
        .post(format!("{}/auth/register", h.base))
        .header("origin", "https://hub.example")
        .json(&json!({"email":email,"password":"correct-test-password"}))
        .send()
        .await?;
    let set = cookie(&registered).unwrap();
    assert!(set.contains("; Path=/auth;") && set.contains("; HttpOnly; SameSite=Strict; Secure"));
    let first = token(&set);
    // Without an HTTPS page the cookie cannot be Secure, or a plain-HTTP dev setup loses it.
    let plain = refresh(first.clone()).await?;
    assert_eq!(plain.status(), 200);
    let set = cookie(&plain).unwrap();
    assert!(!set.contains("Secure"));
    let second = token(&set);
    let access: Value = plain.json().await?;
    assert_eq!(access["email"], email.as_str());
    assert_eq!(me(access).await?.status(), 200);
    // A tab that refreshed with the old token at the same moment still gets in.
    let concurrent = refresh(first.clone()).await?;
    assert_eq!(concurrent.status(), 200);
    assert!(cookie(&concurrent).is_none());
    let third = refresh(second.clone()).await?;
    let latest = token(&cookie(&third).unwrap());
    let access: Value = third.json().await?;
    // After the grace a replaced token is a replay: the device signs out everywhere.
    sqlx::query("UPDATE auth_refresh SET rotated_at=now()-interval '2 minutes'")
        .execute(&h.db)
        .await?;
    assert_eq!(refresh(second).await?.status(), 401);
    assert_eq!(refresh(latest).await?.status(), 401);
    assert_eq!(me(access).await?.status(), 401);
    assert_eq!(refresh("unknown".into()).await?.status(), 401);
    assert_eq!(
        http.post(format!("{}/auth/refresh", h.base))
            .send()
            .await?
            .status(),
        401
    );
    // Logging out ends the device and clears its cookie.
    let login = http
        .post(format!("{}/auth/login", h.base))
        .json(&json!({"email":email,"password":"correct-test-password"}))
        .send()
        .await?;
    let device = token(&cookie(&login).unwrap());
    let access: Value = login.json().await?;
    let out = http
        .post(format!("{}/auth/logout", h.base))
        .bearer_auth(access["token"].as_str().unwrap())
        .send()
        .await?;
    assert_eq!(out.status(), 204);
    assert!(
        cookie(&out)
            .unwrap()
            .starts_with("ryzhub_refresh=; Path=/auth; Max-Age=0;")
    );
    assert_eq!(refresh(device).await?.status(), 401);
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn member_removal_revokes_org_access_and_stops_waiting_run() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let admin = h.user().await?;
    let member = h.user().await?;
    let org = id(&admin, "org_id");
    let user = id(&member, "user_id");
    sqlx::query("INSERT INTO memberships(org_id,user_id,role) VALUES($1,$2,'member')")
        .bind(org)
        .bind(user)
        .execute(&h.db)
        .await?;
    sqlx::query("INSERT INTO channels(id,org_id,kind,user_id) VALUES($1,$2,'user',$3)")
        .bind(format!("user:{org}:{user}"))
        .bind(org)
        .bind(user)
        .execute(&h.db)
        .await?;
    let ctx = h.app.store.context(user, org).await?;
    let conv = h
        .create(&ctx, "待回答任务", "chat", json!({}), None)
        .await?
        .id;
    let channel = format!("conv:{conv}");
    let mut owner = h.owner()?;
    let mut socket = connect(&h.base, &member, "member-device").await?;
    subscribe(&mut socket, &channel, None).await?;
    send(
        &mut socket,
        frame(&channel, Body::Command(message(Uuid::now_v7(), "question"))),
    )
    .await?;
    until(&mut socket, |f| {
        matches!(event_of(f), Some(Event::PendingActionChanged(_)))
    })
    .await?;
    reqwest::Client::new()
        .delete(format!("{}/orgs/{org}/members/{user}", h.base))
        .bearer_auth(admin["token"].as_str().unwrap())
        .send()
        .await?
        .error_for_status()?;
    until(&mut socket, |f| {
        matches!(f.body, Some(Body::SubscriptionRevoked(_)))
    })
    .await?;
    // The session survives for the member's other organizations; this one is gone, to REST
    // and to commands alike.
    h.app
        .store
        .authenticate(member["token"].as_str().unwrap())
        .await?;
    assert!(h.app.store.context(user, org).await.is_err());
    let http = reqwest::Client::new();
    let base = format!("{}/orgs/{org}", h.base);
    for request in [
        http.get(format!("{base}/conversations")),
        http.post(format!("{base}/memories"))
            .json(&json!({"content": "after removal"})),
        http.get(format!(
            "{base}/conversations/{conv}/memory-context/{}",
            Uuid::now_v7()
        )),
    ] {
        let response = request
            .bearer_auth(member["token"].as_str().unwrap())
            .send()
            .await?;
        assert_eq!(response.status(), 403);
    }
    let user_channel = format!("user:{org}:{user}");
    for (channel, command) in [
        (&channel, message(Uuid::now_v7(), "after removal")),
        (
            &user_channel,
            create_run(Uuid::now_v7(), Uuid::now_v7(), "after removal"),
        ),
    ] {
        let cid = Uuid::parse_str(&command.cid)?;
        send(&mut socket, frame(channel, Body::Command(command))).await?;
        assert!(matches!(
            result_of(&mut socket, cid).await?,
            Body::CommandRejected(r) if r.code == "FORBIDDEN"
        ));
    }
    let mut stopped = false;
    for _ in 0..200 {
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE conversation_id=$1")
            .bind(conv)
            .fetch_one(&h.db)
            .await?;
        if state == "cancelled" {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(stopped);
    let owner_id: Uuid = sqlx::query_scalar("SELECT creator_id FROM conversations WHERE id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(owner_id, user);
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn delayed_nats_frames_from_retired_publisher_are_discarded() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    let channel = format!("conv:{conv}");
    let old = h
        .app
        .store
        .claim(org, conv, Uuid::now_v7(), 3000)
        .await?
        .unwrap();
    sqlx::query("UPDATE channels SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(&channel)
        .execute(&h.db)
        .await?;
    let current = h
        .app
        .store
        .claim(org, conv, Uuid::now_v7(), 3000)
        .await?
        .unwrap();
    let mut socket = connect(&h.base, &user, "fencing-client").await?;
    subscribe(&mut socket, &channel, None).await?;
    until(&mut socket, |f| {
        matches!(f.body, Some(Body::SubscriptionSynced(_)))
    })
    .await?;
    for (epoch, text) in [
        (old.epoch, "retired publisher"),
        (current.epoch, "current publisher"),
    ] {
        let mut f = frame(
            &channel,
            Body::Delta(Delta {
                item_id: "test".into(),
                stream: "text".into(),
                text: text.into(),
                ..Default::default()
            }),
        );
        f.writer_epoch = epoch as u64;
        h.app
            .nats
            .publish(owner::subject(&channel), f.encode_to_vec().into())
            .await?;
    }
    h.app.nats.flush().await?;
    let received = next(&mut socket).await?;
    assert!(matches!(received.body,Some(Body::Delta(d)) if d.text=="current publisher"));
    socket.close(None).await?;
    h.close().await
}

/// What the Owner holding `lease`, configured by `config`, answers to the user's `envelope`.
async fn processed(
    h: &Harness,
    ctx: &OrgContext,
    lease: &Lease,
    config: &Config,
    envelope: &CommandEnvelope,
) -> anyhow::Result<Body> {
    let store = &h.app.store;
    let conv = lease.conversation_id;
    store
        .accept(ctx.org_id, ctx.user_id, conv, "review", envelope)
        .await?;
    owner::process_with_config(store, lease, config).await?;
    let cid = Uuid::parse_str(&envelope.cid)?;
    let result = store.command_result(ctx.org_id, ctx.user_id, cid).await?;
    result.and_then(|f| f.body).context("unprocessed command")
}

/// The run of a conversation that has one.
async fn only_run(h: &Harness, conv: Uuid) -> anyhow::Result<Uuid> {
    Ok(
        sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1")
            .bind(conv)
            .fetch_one(&h.db)
            .await?,
    )
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn parallel_approvals_each_resolve_and_the_run_resumes_after_the_last() -> anyhow::Result<()>
{
    let h = Harness::new().await?;
    let outcome = async {
        let user = h.user().await?;
        let org = id(&user, "org_id");
        let conv = h.conversation(&user).await?;
        let ctx = h.context(&user).await?;
        let lease = h
            .app
            .store
            .claim(org, conv, Uuid::now_v7(), 60000)
            .await?
            .unwrap();
        let config = &h.app.config;
        let turn = message(Uuid::now_v7(), "parallel approval");
        processed(&h, &ctx, &lease, config, &turn).await?;
        let run = only_run(&h, conv).await?;
        let actions = [Uuid::now_v7(), Uuid::now_v7()];
        for action in actions {
            sqlx::query(
                "INSERT INTO pending_actions(id,org_id,run_id,run_epoch,kind,summary,options,
                   expires_at)
                 VALUES($1,$2,$3,$4,'approval','parallel MCP approval',ARRAY['allow','deny'],
                   now()+interval '10 minutes')",
            )
            .bind(action)
            .bind(org)
            .bind(run)
            .bind(lease.epoch)
            .execute(&h.db)
            .await?;
        }
        sqlx::query("UPDATE runs SET state='requires_action' WHERE id=$1")
            .bind(run)
            .execute(&h.db)
            .await?;
        for (index, action) in actions.into_iter().enumerate() {
            let decision = DecideApproval {
                action_id: action.to_string(),
                run_epoch: lease.epoch as u64,
                decision: "allow".into(),
            };
            let envelope = CommandEnvelope {
                cid: Uuid::now_v7().to_string(),
                command: Some(Command::DecideApproval(decision)),
            };
            let reply = processed(&h, &ctx, &lease, config, &envelope).await?;
            ensure!(
                matches!(reply, Body::CommandAck(_)),
                "approval {index} was refused while pending: {reply:?}"
            );
            let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=$1")
                .bind(run)
                .fetch_one(&h.db)
                .await?;
            ensure!(
                state
                    == if index == 0 {
                        "requires_action"
                    } else {
                        "running"
                    }
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn a_failed_restore_blocks_new_runs_until_its_retry_completes_once() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let temp = tempfile::tempdir()?;
    let socket = temp.path().join("host.sock");
    let mut restored = false;
    let (stub, requests) = recording_host(&socket, move |command| {
        if command["kind"] == "restore" && !restored {
            restored = true;
            // Destination was created, but the Owner lost its lease before DB completion.
            Err("FENCED")
        } else if command["kind"] == "create" {
            Err("REVIEW_STOP_BEFORE_CREATE")
        } else {
            Ok(json!({"stopped":true}))
        }
    })?;
    let created = || {
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "create")
    };
    let outcome = async {
        let user = h.user().await?;
        let org = id(&user, "org_id");
        let conv = h.conversation(&user).await?;
        sqlx::query("UPDATE conversations SET mode='work',run_config='{}' WHERE id=$1")
            .bind(conv)
            .execute(&h.db)
            .await?;
        let ctx = h.context(&user).await?;
        let lease = h
            .app
            .store
            .claim(org, conv, Uuid::now_v7(), 60000)
            .await?
            .unwrap();
        let mut config = Config::default();
        config.llm.enabled = true;
        config.work.enabled = true;
        config.work.model = "gpt-5.4".into();
        config.sandbox.enabled = true;
        config.sandbox.host_socket = socket.to_str().unwrap().into();
        config.sandbox.image = format!("review@sha256:{}", "b".repeat(64));
        let storage = temp.path().join("missing-blobs");
        config.work.storage_root = storage.to_str().unwrap().into();
        config.work.secondary_root = None;
        let turn = message(Uuid::now_v7(), "first turn");
        processed(&h, &ctx, &lease, &config, &turn).await?;
        let run = only_run(&h, conv).await?;
        sqlx::query("UPDATE runs SET state='completed',ended_at=now() WHERE id=$1")
            .bind(run)
            .execute(&h.db)
            .await?;
        let sandbox = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,
               handle,persistent,native_thread_id,node_lease_until)
             VALUES($1,$2,$3,$4,1,$5,'running','{}',true,'original-thread',
               now()+interval '15 seconds')",
        )
        .bind(sandbox)
        .bind(org)
        .bind(conv)
        .bind(run)
        .bind(lease.epoch)
        .execute(&h.db)
        .await?;
        sqlx::query("UPDATE runs SET sandbox_id=$2 WHERE id=$1")
            .bind(run)
            .bind(sandbox)
            .execute(&h.db)
            .await?;
        let checkpoint = Uuid::now_v7();
        use sha2::Digest;
        let bytes = b"checkpoint fixture";
        let checksum = format!("{:x}", sha2::Sha256::digest(bytes));
        let manifest = json!({"image":config.sandbox.image,"format":1,"encoding":"tar.zst",
            "harness":"codex","harness_version":"0.154.0","sha256":checksum,
            "thread_id":"original-thread"});
        sqlx::query(
            "INSERT INTO checkpoints(id,org_id,conversation_id,sandbox_id,manifest,channel_seq,gseq)
             VALUES($1,$2,$3,$4,$5,0,0)",
        )
        .bind(checkpoint)
        .bind(org)
        .bind(conv)
        .bind(sandbox)
        .bind(manifest)
        .execute(&h.db)
        .await?;
        sqlx::query(
            "INSERT INTO workspace_heads(org_id,conversation_id,checkpoint_id) VALUES($1,$2,$3)",
        )
        .bind(org)
        .bind(conv)
        .bind(checkpoint)
        .execute(&h.db)
        .await?;
        let job = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO workspace_jobs(id,org_id,conversation_id,kind) VALUES($1,$2,$3,'restore')",
        )
        .bind(job)
        .bind(org)
        .bind(conv)
        .execute(&h.db)
        .await?;
        let app = App::new(h.app.store.clone(), h.app.nats.clone(), config.clone())?;
        let link = OwnerLink::detached();
        let job_state = || {
            sqlx::query_scalar::<_, String>("SELECT state FROM workspace_jobs WHERE id=$1")
                .bind(job)
                .fetch_one(&h.db)
        };
        ryzhub_server::workspace::jobs(&app, &lease, &link).await?;
        ensure!(
            job_state().await? == "failed",
            "expected unavailable blob to fail restore"
        );
        let turn = message(Uuid::now_v7(), "continue after failed restore");
        let response = processed(&h, &ctx, &lease, &config, &turn).await?;
        let Body::CommandRejected(refused) = &response else {
            anyhow::bail!("message must fail before creating a run: {response:?}")
        };
        ensure!(refused.code == "WORKSPACE_RECOVERY_REQUIRED", "{refused:?}");
        ensure!(
            !created(),
            "failed restore allowed creation of a new blank workspace"
        );
        let mut headers = axum::http::HeaderMap::new();
        let bearer = format!("Bearer {}", user["token"].as_str().unwrap());
        headers.insert("Authorization", bearer.parse()?);
        let input = ryzhub_server::workspace::JobInput {
            kind: "restore".into(),
        };
        let retried = ryzhub_server::workspace::enqueue(
            axum::extract::State(app.clone()),
            headers,
            axum::extract::Path((org, conv)),
            axum::Json(input),
        )
        .await
        .map_err(|e| e.0)?;
        ensure!(
            retried.0["id"] == job.to_string(),
            "retry must reuse restore destination"
        );
        ryzhub_server::workspace::blob::BlobStore::configured(&config.work.storage_root)?
            .scoped(org)
            .put(&checksum, bytes)
            .await?;
        let interrupted = ryzhub_server::workspace::jobs(&app, &lease, &link)
            .await
            .unwrap_err();
        ensure!(
            ryzhub_server::error::is_fenced(&interrupted),
            "expected FENCED, got {interrupted:#}"
        );
        sqlx::query(
            "UPDATE channels SET lease_until=now()-interval '1 second'
             WHERE org_id=$1 AND conversation_id=$2",
        )
        .bind(org)
        .bind(conv)
        .execute(&h.db)
        .await?;
        let successor = h
            .app
            .store
            .claim(org, conv, Uuid::now_v7(), 60000)
            .await?
            .unwrap();
        ryzhub_server::workspace::jobs(&app, &successor, &link).await?;
        ensure!(job_state().await? == "completed");
        let stopped = requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == "stop")
            .all(|r| r["sandbox_id"] == sandbox.to_string());
        ensure!(stopped, "retry stopped destination");
        let generation: i64 =
            sqlx::query_scalar("SELECT generation FROM workspace_heads WHERE conversation_id=$1")
                .bind(conv)
                .fetch_one(&h.db)
                .await?;
        ensure!(generation == 2, "retry incremented generation again");
        let last_seq = || {
            sqlx::query_scalar::<_, i64>("SELECT last_seq FROM channels WHERE conversation_id=$1")
                .bind(conv)
                .fetch_one(&h.db)
        };
        let seq = last_seq().await?;
        // A crash after applying but before completing the job must not append again.
        ryzhub_server::workspace::restore(&app, &successor, job, &link).await?;
        ensure!(seq == last_seq().await?);
        let mut tx = h.app.store.tx(org).await?;
        ryzhub_server::work::available(&mut tx, &successor).await?;
        tx.commit().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stub.abort();
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn sync_projects_turn_events_before_acknowledging_them() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let temp = tempfile::tempdir()?;
    let socket = temp.path().join("host.sock");
    let (stub, requests) = recording_host(&socket, |request| {
        let command = &request["request"]["command"];
        Ok(if command["kind"] == "replay" {
            let event =
                |gseq: u64, result: Value| json!({"gseq":gseq,"op_id":"native","result":result});
            let events = vec![
                event(
                    1,
                    json!({"type":"item","phase":"completed","turn_id":"new-turn",
                        "native_id":"new-native-item","kind":"assistant",
                        "content":{"text":"NEW_RUN_CONTENT"}}),
                ),
                event(
                    2,
                    json!({"type":"question_requested","request_id":"17","turn_id":"new-turn",
                        "questions":[{"id":"color","question":"Which color?"}]}),
                ),
                event(
                    3,
                    json!({"type":"turn_started","turn_id":"restored-historical-turn"}),
                ),
            ];
            let events: Vec<_> = events
                .into_iter()
                .filter(|e| e["gseq"].as_u64().unwrap() > command["after"].as_u64().unwrap())
                .collect();
            json!({"events":events,"head":3,"folds":{}})
        } else {
            json!({"acked":1})
        })
    })?;
    // The key the host was last given.
    let rotated = || {
        let requests = requests.lock().unwrap();
        let mut commands = requests.iter().rev().map(|r| &r["request"]["command"]);
        let rotation = commands.find(|c| c["kind"] == "rotate_key").unwrap();
        rotation["token"].as_str().unwrap().to_owned()
    };
    let outcome = async {
        let user = h.user().await?;
        let org = id(&user, "org_id");
        let conv = h.conversation(&user).await?;
        let ctx = h.context(&user).await?;
        let lease = h
            .app
            .store
            .claim(org, conv, Uuid::now_v7(), 60000)
            .await?
            .unwrap();
        let mut config = Config::default();
        config.llm.enabled = true;
        config.llm.model = "fixture-model".into();
        config.llm.unix_socket = Some("/gateway/test.sock".into());
        config.work.model = "gpt-5.4".into();
        config.sandbox.enabled = true;
        config.sandbox.host_socket = socket.to_str().unwrap().into();
        processed(
            &h,
            &ctx,
            &lease,
            &config,
            &message(Uuid::now_v7(), "old turn"),
        )
        .await?;
        let old = only_run(&h, conv).await?;
        sqlx::query(
            "UPDATE runs SET state='completed',native_turn_id='old-turn',ended_at=now()
             WHERE id=$1",
        )
        .bind(old)
        .execute(&h.db)
        .await?;
        processed(
            &h,
            &ctx,
            &lease,
            &config,
            &message(Uuid::now_v7(), "new turn"),
        )
        .await?;
        let new: Uuid =
            sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1 AND state='running'")
                .bind(conv)
                .fetch_one(&h.db)
                .await?;
        let sandbox = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,
               handle,persistent)
             VALUES($1,$2,$3,$4,1,$5,'running','{}',true)",
        )
        .bind(sandbox)
        .bind(org)
        .bind(conv)
        .bind(new)
        .bind(lease.epoch)
        .execute(&h.db)
        .await?;
        sqlx::query("UPDATE runs SET sandbox_id=$2 WHERE id=ANY($1)")
            .bind(vec![old, new])
            .bind(sandbox)
            .execute(&h.db)
            .await?;
        let app = App::new(h.app.store.clone(), h.app.nats.clone(), config)?;
        let link = OwnerLink::detached();
        let acked = || {
            sqlx::query_scalar::<_, i64>("SELECT acked_gseq FROM sandboxes WHERE id=$1")
                .bind(sandbox)
                .fetch_one(&h.db)
        };
        // The event's turn is not registered yet: projection waits instead of acknowledging it.
        let sync = ryzhub_server::work::sync_through(&app, &lease, old, sandbox, 1, &link);
        let waiting = tokio::time::timeout(Duration::from_millis(500), sync).await;
        ensure!(
            waiting.is_err(),
            "sync must wait for the turn to be registered"
        );
        ensure!(
            acked().await? == 0,
            "unregistered turn must not be acknowledged"
        );
        sqlx::query("UPDATE runs SET native_turn_id='new-turn' WHERE id=$1")
            .bind(new)
            .execute(&h.db)
            .await?;
        ryzhub_server::work::sync_through(&app, &lease, old, sandbox, 1, &link).await?;
        ryzhub_server::work::sync_through(&app, &lease, new, sandbox, 3, &link).await?;
        let projected: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM items WHERE run_id=$1 AND text='NEW_RUN_CONTENT'",
        )
        .bind(new)
        .fetch_one(&h.db)
        .await?;
        ensure!(
            acked().await? == 3,
            "historical usage notification must not block replay after restore"
        );
        ensure!(
            projected == 1,
            "checkpoint consumed and acknowledged a newer turn event without projecting it"
        );
        let (action, kind): (Uuid, String) =
            sqlx::query_as("SELECT id,kind FROM pending_actions WHERE run_id=$1")
                .bind(new)
                .fetch_one(&h.db)
                .await?;
        ensure!(
            kind == "question",
            "a Work question must render an answer field"
        );
        // The key rotates while the run waits for the member, without starting a new turn.
        ryzhub_server::work::refresh_key(&app, &lease, new, sandbox).await?;
        let first = rotated();
        ryzhub_server::work::refresh_key(&app, &lease, new, sandbox).await?;
        let second = rotated();
        ensure!(first != second);
        let second = ryzhub_server::llm::keys::resolve(&h.app.store, &second).await?;
        let mut tx = h.app.store.tx(org).await?;
        ryzhub_server::llm::gateway::validate_key(&mut tx, &second, false).await?;
        tx.commit().await?;
        let answer = AnswerQuestion {
            action_id: action.to_string(),
            run_epoch: lease.epoch as u64,
            answer: "green".into(),
        };
        let envelope = CommandEnvelope {
            cid: Uuid::now_v7().to_string(),
            command: Some(Command::AnswerQuestion(answer)),
        };
        let response = processed(&h, &ctx, &lease, &app.config, &envelope).await?;
        ensure!(matches!(response, Body::CommandAck(_)));
        let answer: Value =
            sqlx::query_scalar("SELECT answer FROM native_requests WHERE action_id=$1")
                .bind(action)
                .fetch_one(&h.db)
                .await?;
        ensure!(
            answer == json!({"type":"question","answers":{"color":"green"}}),
            "unexpected answer {answer}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stub.abort();
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn legacy_upgrade_preserves_native_model_and_existing_settings() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let outcome = async {
        let user = h.user().await?;
        let ctx = h.context(&user).await?;
        let backfill = include_str!("../../../migrations/0011_legacy_run_settings.sql");
        for mode in ["chat", "work"] {
            let conv = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO conversations(id,org_id,creator_id,title,mode)
                 VALUES($1,$2,$3,'Legacy upgrade',$4)",
            )
            .bind(conv)
            .bind(ctx.org_id)
            .bind(ctx.user_id)
            .bind(mode)
            .execute(&h.db)
            .await?;
            let run = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO runs(id,org_id,conversation_id,harness,model,exec_epoch,state,
                   response_text,assistant_item_id,cause_cid)
                 VALUES($1,$2,$3,$4,'pinned-legacy-model',1,'running','',$5,$6)",
            )
            .bind(run)
            .bind(ctx.org_id)
            .bind(conv)
            .bind(if mode == "work" { "codex" } else { "chat" })
            .bind(Uuid::now_v7())
            .bind(Uuid::now_v7())
            .execute(&h.db)
            .await?;
            sqlx::raw_sql(backfill).execute(&h.db).await?;
            let (protocol, settings): (String, Value) =
                sqlx::query_as("SELECT protocol,run_config FROM runs WHERE id=$1")
                    .bind(run)
                    .fetch_one(&h.db)
                    .await?;
            ensure!(settings["model"] == "pinned-legacy-model");
            ensure!(
                protocol
                    == if mode == "work" {
                        "responses"
                    } else {
                        "anthropic"
                    }
            );
            let stored: Value =
                sqlx::query_scalar("SELECT run_config FROM conversations WHERE id=$1")
                    .bind(conv)
                    .fetch_one(&h.db)
                    .await?;
            ensure!(stored == settings);
            sqlx::query(
                "UPDATE conversations SET run_config=jsonb_set(run_config,'{reasoning}','\"high\"')
                 WHERE id=$1",
            )
            .bind(conv)
            .execute(&h.db)
            .await?;
            sqlx::raw_sql(backfill).execute(&h.db).await?;
            let reasoning: String = sqlx::query_scalar(
                "SELECT run_config->>'reasoning' FROM conversations WHERE id=$1",
            )
            .bind(conv)
            .fetch_one(&h.db)
            .await?;
            ensure!(
                reasoning == "high",
                "backfill cannot overwrite configured conversations"
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    h.close().await?;
    outcome
}

/// Every request a stub host received.
type Recorded = std::sync::Arc<std::sync::Mutex<Vec<Value>>>;

/// A host agent stub on `socket` that records every request and answers it with `reply`.
fn recording_host(
    socket: &std::path::Path,
    mut reply: impl FnMut(&Value) -> Result<Value, &'static str> + Send + 'static,
) -> anyhow::Result<(tokio::task::JoinHandle<()>, Recorded)> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::UnixListener::bind(socket)?;
    let requests = Recorded::default();
    let seen = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = tokio::io::BufReader::new(read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            // An upload's content follows its request.
            if request["kind"] == "artifact_upload" {
                let mut body = vec![0; request["size"].as_u64().unwrap() as usize];
                reader.read_exact(&mut body).await.unwrap();
            }
            let reply = match reply(&request) {
                Ok(value) => json!({"ok":true,"value":value,"error":null}),
                Err(error) => json!({"ok":false,"value":null,"error":error}),
            };
            seen.lock().unwrap().push(request);
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    Ok((task, requests))
}

/// A Work configuration pointed at a stub host.
fn work_config(socket: &std::path::Path, storage: &std::path::Path) -> Config {
    let mut config = Config::default();
    config.llm.enabled = true;
    config.llm.unix_socket = Some("/gateway/test.sock".into());
    config.work.enabled = true;
    config.work.model = "gpt-5.4".into();
    config.work.storage_root = storage.to_str().unwrap().into();
    config.work.secondary_root = None;
    config.sandbox.enabled = true;
    config.sandbox.host_socket = socket.to_str().unwrap().into();
    config.sandbox.image = format!("review@sha256:{}", "c".repeat(64));
    config
}

/// A Work conversation of the user's whose first turn the Owner holding the lease accepted,
/// configured by `config`; its run.
async fn work_turn(
    h: &Harness,
    user: &Value,
    config: &Config,
) -> anyhow::Result<(Uuid, Lease, Uuid)> {
    let conv = h.conversation(user).await?;
    sqlx::query("UPDATE conversations SET mode='work',run_config='{}' WHERE id=$1")
        .bind(conv)
        .execute(&h.db)
        .await?;
    let ctx = h.context(user).await?;
    let lease = h
        .app
        .store
        .claim(ctx.org_id, conv, Uuid::now_v7(), 60_000)
        .await?
        .unwrap();
    processed(h, &ctx, &lease, config, &message(Uuid::now_v7(), "turn")).await?;
    let run = only_run(h, conv).await?;
    Ok((conv, lease, run))
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn runs_wait_for_idle_suspension_and_models_are_discovered_once() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let temp = tempfile::tempdir()?;
    let socket = temp.path().join("host.sock");
    let (stub, requests) = recording_host(&socket, |_| {
        Ok(json!({"state":"completed","result":{"models":[]}}))
    })?;
    let outcome = async {
        let user = h.user().await?;
        let org = id(&user, "org_id");
        let config = work_config(&socket, temp.path());
        let (conv, lease, run) = work_turn(&h, &user, &config).await?;
        sqlx::query(
            "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,
               handle,persistent,node_lease_until)
             VALUES($1,$2,$3,$4,1,$5,'running','{}',true,now()+interval '1 hour')",
        )
        .bind(Uuid::now_v7())
        .bind(org)
        .bind(conv)
        .bind(run)
        .bind(lease.epoch)
        .execute(&h.db)
        .await?;
        let suspension = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO workspace_jobs(id,org_id,conversation_id,kind,state)
             VALUES($1,$2,$3,'suspend','running')",
        )
        .bind(suspension)
        .bind(org)
        .bind(conv)
        .execute(&h.db)
        .await?;
        let app = App::new(h.app.store.clone(), h.app.nats.clone(), config)?;
        // The suspension is about to stop the sandbox: the new run must not start using it.
        let ensure_work = || ryzhub_server::work::ensure_work(&app, &lease, run);
        let waiting = tokio::time::timeout(Duration::from_millis(1500), ensure_work()).await;
        ensure!(
            waiting.is_err(),
            "a run must wait for an in-progress suspension"
        );
        ensure!(
            requests.lock().unwrap().is_empty(),
            "nothing may touch the sandbox while it is being suspended"
        );
        sqlx::query("UPDATE workspace_jobs SET state='completed' WHERE id=$1")
            .bind(suspension)
            .execute(&h.db)
            .await?;
        // Later runs reuse the recorded models: the guest forgets old results eventually.
        ensure_work().await?;
        ensure_work().await?;
        let discoveries = requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|r| r["request"]["command"]["op_id"].as_str())
            .filter(|op| op.ends_with(":models"))
            .count();
        ensure!(discoveries == 1, "model discovery ran {discoveries} times");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stub.abort();
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn suspended_workspace_checkpoints_forks_and_archives_from_its_head() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let temp = tempfile::tempdir()?;
    let socket = temp.path().join("host.sock");
    let (stub, requests) = recording_host(&socket, |_| {
        Ok(json!({"state":"completed","result":{"models":[]}}))
    })?;
    let outcome = async {
        let user = h.user().await?;
        let org = id(&user, "org_id");
        let config = work_config(&socket, temp.path());
        let (conv, lease, run) = work_turn(&h, &user, &config).await?;
        sqlx::query("UPDATE runs SET state='completed',ended_at=now() WHERE id=$1")
            .bind(run)
            .execute(&h.db)
            .await?;
        let sandbox = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,
               handle,persistent,native_thread_id)
             VALUES($1,$2,$3,$4,1,$5,'suspended','{}',true,'thread')",
        )
        .bind(sandbox)
        .bind(org)
        .bind(conv)
        .bind(run)
        .bind(lease.epoch)
        .execute(&h.db)
        .await?;
        let head = Uuid::now_v7();
        let manifest = json!({"image":config.sandbox.image,"format":1,"encoding":"tar.zst",
            "sha256":"d".repeat(64),"thread_id":"thread"});
        sqlx::query(
            "INSERT INTO checkpoints(id,org_id,conversation_id,sandbox_id,manifest,channel_seq,gseq)
             VALUES($1,$2,$3,$4,$5,0,0)",
        )
        .bind(head)
        .bind(org)
        .bind(conv)
        .bind(sandbox)
        .bind(manifest)
        .execute(&h.db)
        .await?;
        sqlx::query(
            "INSERT INTO workspace_heads(org_id,conversation_id,checkpoint_id) VALUES($1,$2,$3)",
        )
        .bind(org)
        .bind(conv)
        .bind(head)
        .execute(&h.db)
        .await?;
        let app = App::new(h.app.store.clone(), h.app.nats.clone(), config)?;
        let link = OwnerLink::detached();
        // A job of `kind`, run once by the Owner; its state.
        let job = |kind: &'static str| {
            let (h, app, lease, link) = (&h, &app, &lease, &link);
            async move {
                let job = Uuid::now_v7();
                sqlx::query(
                    "INSERT INTO workspace_jobs(id,org_id,conversation_id,kind)
                     VALUES($1,$2,$3,$4)",
                )
                .bind(job)
                .bind(org)
                .bind(conv)
                .bind(kind)
                .execute(&h.db)
                .await?;
                ryzhub_server::workspace::jobs(app, lease, link).await?;
                let state: String =
                    sqlx::query_scalar("SELECT state FROM workspace_jobs WHERE id=$1")
                        .bind(job)
                        .fetch_one(&h.db)
                        .await?;
                anyhow::Ok(state)
            }
        };
        // A manual checkpoint of a suspended workspace: its head is already current.
        ensure!(
            job("checkpoint").await? == "completed",
            "checkpoint of a suspended workspace failed"
        );
        // A fork starts from that head, with a pin of its own.
        let fork = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO conversation_operations(id,org_id,conversation_id,kind,payload)
             VALUES($1,$2,$3,'fork','{\"seq\":0}')",
        )
        .bind(fork)
        .bind(org)
        .bind(conv)
        .execute(&h.db)
        .await?;
        ryzhub_server::conversation_commands::jobs(&app, &lease, &link).await?;
        let forked: String =
            sqlx::query_scalar("SELECT state FROM conversation_operations WHERE id=$1")
                .bind(fork)
                .fetch_one(&h.db)
                .await?;
        ensure!(
            forked == "completed",
            "fork of a suspended workspace failed"
        );
        let pinned: Uuid = sqlx::query_scalar("SELECT checkpoint_id FROM restore_pins WHERE id=$1")
            .bind(fork)
            .fetch_one(&h.db)
            .await?;
        ensure!(pinned == head, "the fork must pin the suspended head");
        // Archiving needs no new checkpoint either.
        ensure!(
            job("archive").await? == "completed",
            "archive of a suspended workspace failed"
        );
        let states: (String, String) = sqlx::query_as(
            "SELECT c.state,s.state FROM conversations c
             JOIN sandboxes s ON s.conversation_id=c.id WHERE c.id=$1",
        )
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
        ensure!(
            states == ("archived".into(), "archived".into()),
            "unexpected states {states:?}"
        );
        ensure!(
            requests.lock().unwrap().is_empty(),
            "a suspended workspace needs no execution node"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stub.abort();
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn owner_discovery_applies_the_organization_idle_limit() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let worker = PgPool::connect(&h.worker_url).await?;
    let outcome = async {
        let user = h.user().await?;
        let org = id(&user, "org_id");
        let conv = h.conversation(&user).await?;
        sqlx::query("UPDATE conversations SET mode='work' WHERE id=$1")
            .bind(conv)
            .execute(&h.db)
            .await?;
        // Idle for ten minutes: past a 60-second organization limit, within the global 1800.
        let run = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO runs(id,org_id,conversation_id,harness,model,exec_epoch,state,
               response_text,assistant_item_id,cause_cid,ended_at)
             VALUES($1,$2,$3,'codex','gpt-5.4',1,'cancelled','',$4,$5,
               now()-interval '10 minutes')",
        )
        .bind(run)
        .bind(org)
        .bind(conv)
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .execute(&h.db)
        .await?;
        sqlx::query(
            "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,
               handle,persistent,node_lease_until,created_at)
             VALUES($1,$2,$3,$4,1,1,'running','{}',true,now()+interval '1 hour',
               now()-interval '10 minutes')",
        )
        .bind(Uuid::now_v7())
        .bind(org)
        .bind(conv)
        .bind(run)
        .execute(&h.db)
        .await?;
        let woken = |config: Config| {
            let worker = worker.clone();
            async move {
                let active = ryzhub_server::owner::active_conversations(&worker, &config).await?;
                Ok::<_, anyhow::Error>(active.contains(&(org, conv)))
            }
        };
        let global = Config::default();
        let mut disabled = Config::default();
        disabled.work.idle_suspend_secs = 0;
        ensure!(
            !woken(global.clone()).await?,
            "within the global idle limit"
        );
        sqlx::query(
            "INSERT INTO org_policies(org_id,version,policy)
             VALUES($1,(SELECT coalesce(max(version),0)+1 FROM org_policies WHERE org_id=$1),
               '{\"compute_idle_ttl_secs\":60}')",
        )
        .bind(org)
        .execute(&h.db)
        .await?;
        ensure!(
            woken(global).await?,
            "the organization's shorter limit must wake the workspace"
        );
        ensure!(
            woken(disabled.clone()).await?,
            "an organization limit applies without a global one"
        );
        sqlx::query("DELETE FROM org_policies WHERE org_id=$1")
            .bind(org)
            .execute(&h.db)
            .await?;
        ensure!(!woken(disabled).await?, "no limit at all: never suspend");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    worker.close().await;
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn usage_partitions_move_stranded_rows_and_retention_drops_whole_months() -> anyhow::Result<()>
{
    let h = Harness::new().await?;
    let user = h.user().await?;
    let (org, uid) = (id(&user, "org_id"), id(&user, "user_id"));
    let insert =
        "INSERT INTO usage_records(id,org_id,user_id,model,created_at) VALUES($1,$2,$3,'m',$4)";
    let (old, current) = (Uuid::now_v7(), Uuid::now_v7());
    // No partition covers January 2025, so the row lands in the default partition.
    let old_at = chrono::DateTime::parse_from_rfc3339("2025-01-15T12:00:00Z")?;
    sqlx::query(insert)
        .bind(old)
        .bind(org)
        .bind(uid)
        .bind(old_at)
        .execute(&h.db)
        .await?;
    sqlx::query(insert)
        .bind(current)
        .bind(org)
        .bind(uid)
        .bind(chrono::Utc::now())
        .execute(&h.db)
        .await?;
    let partition = |row: Uuid| {
        sqlx::query_scalar::<_, String>(
            "SELECT tableoid::regclass::text FROM usage_records WHERE id=$1",
        )
        .bind(row)
        .fetch_optional(&h.db)
    };
    assert_eq!(
        partition(old).await?.as_deref(),
        Some("usage_records_default")
    );
    // Runtime roles reach rows only through the parent table and its row-level security.
    let app = PgPool::connect(&h.app_url).await?;
    let leaf = sqlx::query("SELECT 1 FROM usage_records_default")
        .execute(&app)
        .await;
    assert!(leaf.is_err(), "leaf partitions must not be granted");
    app.close().await;

    let mut config = Config::default();
    ryzhub_server::maintenance::maintain(&h.db, &config).await?;
    assert_eq!(
        partition(old).await?.as_deref(),
        Some("usage_records_2025_01")
    );
    config.retention.usage_days = 30;
    ryzhub_server::maintenance::maintain(&h.db, &config).await?;
    assert_eq!(partition(old).await?, None, "expired month is dropped");
    assert!(partition(current).await?.is_some(), "current month is kept");
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn push_jobs_deduplicate_retry_fence_stale_claims_and_die_after_their_attempts()
-> anyhow::Result<()> {
    use ryzhub_server::push::{self, Job, MAX_ATTEMPTS};
    let h = Harness::new().await?;
    let (user, other) = (h.user().await?, h.user().await?);
    let (org, other_org) = (id(&user, "org_id"), id(&other, "org_id"));
    let mut config = Config::default();
    config.push.enabled = true;
    let app = App::new(h.app.store.clone(), h.app.nats.clone(), config)?;
    let worker = PgPool::connect(&h.worker_url).await?;
    let state = |job: Uuid| {
        sqlx::query_as::<_, (String, i32)>("SELECT state,attempts FROM push_jobs WHERE id=$1")
            .bind(job)
            .fetch_one(&h.db)
    };
    // About a conversation that no longer exists: delivered as nothing to say. Without a
    // user it cannot be delivered at all.
    let gone = json!({"user": id(&user, "user_id"), "conversation": Uuid::now_v7()});
    let broken = json!({});

    let mut tx = h.app.store.tx(org).await?;
    let first = push::enqueue(&mut tx, org, gone.clone(), "k").await?;
    sqlx::query("UPDATE push_jobs SET run_at=now()+interval '1 hour' WHERE id=$1")
        .bind(first)
        .execute(&mut *tx)
        .await?;
    // Merged into the queued job, which becomes due now; the first payload stays.
    let merged = push::enqueue(&mut tx, org, broken.clone(), "k").await?;
    assert_eq!(first, merged);
    let failing = push::enqueue(&mut tx, org, broken.clone(), "failing").await?;
    tx.commit().await?;
    let (payload, due): (Value, bool) =
        sqlx::query_as("SELECT payload,run_at<=now() FROM push_jobs WHERE id=$1")
            .bind(first)
            .fetch_one(&h.db)
            .await?;
    assert!(payload == gone && due);
    // A job enqueued by a transaction that rolls back never exists.
    let mut tx = h.app.store.tx(org).await?;
    let rolled_back = push::enqueue(&mut tx, org, gone.clone(), "rolled back").await?;
    tx.rollback().await?;
    // Organizations see only their own jobs.
    let mut tx = h.app.store.tx(other_org).await?;
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM push_jobs")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    assert_eq!(visible, 0);

    assert_eq!(push::run_due(&app, &worker).await?, 2);
    assert_eq!(state(first).await?, ("completed".into(), 1));
    assert_eq!(
        state(failing).await?,
        ("queued".into(), 1),
        "retried with backoff"
    );
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM push_jobs WHERE id=$1)")
        .bind(rolled_back)
        .fetch_one(&h.db)
        .await?;
    assert!(!exists);
    assert_eq!(push::run_due(&app, &worker).await?, 0, "backoff not over");
    sqlx::query("UPDATE push_jobs SET run_at=now(),attempts=$2-1 WHERE id=$1")
        .bind(failing)
        .bind(MAX_ATTEMPTS)
        .execute(&h.db)
        .await?;
    push::run_due(&app, &worker).await?;
    assert_eq!(state(failing).await?, ("dead".into(), MAX_ATTEMPTS));

    // A worker whose lease expired loses the job: another worker reclaims and completes it,
    // and the first worker's token no longer completes anything.
    let mut tx = h.app.store.tx(org).await?;
    let taken = push::enqueue(&mut tx, org, gone.clone(), "taken").await?;
    tx.commit().await?;
    let stale = Uuid::now_v7();
    let expire = "UPDATE push_jobs SET state='running',attempts=$3,claim_token=$2,
                    lease_until=now()-interval '1 second'
                  WHERE id=$1";
    sqlx::query(expire)
        .bind(taken)
        .bind(stale)
        .bind(1)
        .execute(&h.db)
        .await?;
    assert_eq!(push::run_due(&app, &worker).await?, 1);
    assert_eq!(state(taken).await?, ("completed".into(), 2));
    let mut tx = h.app.store.tx(org).await?;
    let late = Job {
        id: taken,
        org_id: org,
        payload: gone.clone(),
        claim_token: stale,
    };
    assert!(!push::finish(&mut tx, &late).await?);
    tx.commit().await?;

    // A job whose last attempt's lease ran out (its delivery took the process down) is dead
    // rather than retried forever.
    let mut tx = h.app.store.tx(org).await?;
    let crashed = push::enqueue(&mut tx, org, gone, "crashed").await?;
    tx.commit().await?;
    sqlx::query(expire)
        .bind(crashed)
        .bind(Uuid::now_v7())
        .bind(MAX_ATTEMPTS)
        .execute(&h.db)
        .await?;
    assert_eq!(push::run_due(&app, &worker).await?, 0);
    push::sweep(&app, &worker).await?;
    assert_eq!(state(crashed).await?, ("dead".into(), MAX_ATTEMPTS));
    worker.close().await;
    h.close().await
}

fn create_run(cid: Uuid, conv: Uuid, text: &str) -> CommandEnvelope {
    CommandEnvelope {
        cid: cid.to_string(),
        command: Some(Command::CreateRun(CreateRun {
            conversation_id: conv.to_string(),
            mode: "chat".into(),
            first_message: Some(SendMessage {
                text: text.into(),
                ..Default::default()
            }),
            ..Default::default()
        })),
    }
}

/// The result the gateway or the conversation's Owner sent for `cid`.
async fn result_of(socket: &mut Socket, cid: Uuid) -> anyhow::Result<Body> {
    let cid = cid.to_string();
    let f = until(socket, |f| match &f.body {
        Some(Body::CommandAck(a)) => a.cid == cid,
        Some(Body::CommandRejected(r)) => r.cid == cid,
        _ => false,
    })
    .await?;
    Ok(f.body.unwrap())
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS: scripts/test-integration.sh"]
async fn conversations_are_created_by_command_and_users_sync_drafts_reads_and_undos()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let other = h.user().await?;
    let org = id(&user, "org_id");
    let user_channel = format!("user:{org}:{}", id(&user, "user_id"));
    let mut owner = h.owner()?;
    let mut socket = connect(&h.base, &user, "phone").await?;
    subscribe(&mut socket, &user_channel, None).await?;
    // CreateRun makes the conversation and queues its first message; a retry gets the same
    // answer, and the id is no one else's to take.
    let (cid, conv) = (Uuid::now_v7(), Uuid::now_v7());
    send(
        &mut socket,
        frame(
            &user_channel,
            Body::Command(create_run(cid, conv, "离线写下的第一句")),
        ),
    )
    .await?;
    let first = result_of(&mut socket, cid).await?;
    assert!(matches!(first, Body::CommandAck(_)), "{first:?}");
    send(
        &mut socket,
        frame(
            &user_channel,
            Body::Command(create_run(cid, conv, "离线写下的第一句")),
        ),
    )
    .await?;
    assert_eq!(result_of(&mut socket, cid).await?, first);
    let taken = Uuid::now_v7();
    send(
        &mut socket,
        frame(
            &user_channel,
            Body::Command(create_run(taken, conv, "again")),
        ),
    )
    .await?;
    assert!(matches!(
        result_of(&mut socket, taken).await?,
        Body::CommandRejected(r) if r.code == "CONVERSATION_ID_TAKEN" && !r.retryable
    ));
    let mut stranger = connect(&h.base, &other, "laptop").await?;
    let stranger_channel = format!("user:{}:{}", id(&other, "org_id"), id(&other, "user_id"));
    let theirs = Uuid::now_v7();
    send(
        &mut stranger,
        frame(
            &stranger_channel,
            Body::Command(create_run(theirs, conv, "mine")),
        ),
    )
    .await?;
    assert!(matches!(
        result_of(&mut stranger, theirs).await?,
        Body::CommandRejected(r) if r.code == "CONVERSATION_ID_TAKEN"
    ));
    let titled: String = sqlx::query_scalar("SELECT title FROM conversations WHERE id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(titled, "离线写下的第一句");
    // The run starts as for SendMessage; its end marks the conversation as news.
    let channel = format!("conv:{conv}");
    subscribe(&mut socket, &channel, None).await?;
    let mut engine = Engine::default();
    engine.subscribe(channel.clone());
    engine.subscribe(user_channel.clone());
    let mut activity = 0;
    for _ in 0..500 {
        let f = next(&mut socket).await?;
        if let Some(Body::Event(e)) = &f.body
            && let Some(Event::ItemUpserted(item)) = &e.event
            && item.id == conv.to_string()
            && let Some(item_detail::Kind::Conversation(c)) =
                item.detail.as_ref().and_then(|d| d.kind.as_ref())
        {
            activity = c.activity_seq;
        }
        engine.apply(f);
        if activity > 0 {
            break;
        }
    }
    assert!(
        activity > 0,
        "the run's end is published to the user channel"
    );
    let items = &engine.channels[&channel].items;
    assert!(
        items
            .values()
            .any(|i| i.kind == "user" && i.text == "离线写下的第一句")
    );
    // Reading it moves the read position on every device.
    let read = Uuid::now_v7();
    send(
        &mut socket,
        frame(
            &channel,
            Body::Command(CommandEnvelope {
                cid: read.to_string(),
                command: Some(Command::MarkRead(MarkRead { read_seq: activity })),
            }),
        ),
    )
    .await?;
    let read_state = until(
        &mut socket,
        |f| matches!(event_of(f), Some(Event::ReadStateChanged(r)) if r.read_seq == activity),
    )
    .await?;
    assert_eq!(read_state.channel, user_channel);
    // A draft reaches the other devices, and the channel's snapshot until cleared.
    let http = reqwest::Client::new();
    let draft_url = format!("{}/orgs/{org}/conversations/{conv}/draft", h.base);
    http.put(&draft_url)
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"text":"未发送的半句","device_id":"phone"}))
        .send()
        .await?
        .error_for_status()?;
    until(&mut socket, |f| {
        matches!(event_of(f), Some(Event::DraftChanged(d))
            if d.text == "未发送的半句" && d.device_id == "phone")
    })
    .await?;
    let mut tablet = connect(&h.base, &user, "tablet").await?;
    subscribe(&mut tablet, &user_channel, None).await?;
    let snapshot = until(&mut tablet, |f| matches!(f.body, Some(Body::Snapshot(_)))).await?;
    let Some(Body::Snapshot(snapshot)) = snapshot.body else {
        unreachable!()
    };
    assert_eq!(snapshot.drafts[0].text, "未发送的半句");
    assert_eq!(snapshot.read_states[0].read_seq, activity);
    http.put(&draft_url)
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"text":"","device_id":"tablet"}))
        .send()
        .await?
        .error_for_status()?;
    let stranger_put = http
        .put(&draft_url)
        .bearer_auth(other["token"].as_str().unwrap())
        .json(&json!({"text":"x"}))
        .send()
        .await?;
    assert!(!stranger_put.status().is_success());
    let mut later = connect(&h.base, &user, "later").await?;
    subscribe(&mut later, &user_channel, None).await?;
    let snapshot = until(&mut later, |f| matches!(f.body, Some(Body::Snapshot(_)))).await?;
    assert!(matches!(snapshot.body, Some(Body::Snapshot(s)) if s.drafts.is_empty()));
    // A memory change is undone once, whatever retries its command.
    http.patch(format!("{}/orgs/{org}/memories/preferences", h.base))
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"enabled":true,"reference_history":true}))
        .send()
        .await?
        .error_for_status()?;
    let saved: Value = http
        .post(format!("{}/orgs/{org}/memories", h.base))
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"content":"喜欢简洁的回答"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let undo = |cid: Uuid| {
        frame(
            &user_channel,
            Body::Command(CommandEnvelope {
                cid: cid.to_string(),
                command: Some(Command::UndoMemoryUpdate(UndoMemoryUpdate {
                    event_id: saved["event_id"].as_str().unwrap().into(),
                })),
            }),
        )
    };
    let once = Uuid::now_v7();
    send(&mut socket, undo(once)).await?;
    let undone = result_of(&mut socket, once).await?;
    assert!(matches!(&undone, Body::CommandAck(a) if a.result == "undone"));
    send(&mut socket, undo(once)).await?;
    assert_eq!(result_of(&mut socket, once).await?, undone);
    let again = Uuid::now_v7();
    send(&mut socket, undo(again)).await?;
    assert!(matches!(
        result_of(&mut socket, again).await?,
        Body::CommandRejected(r) if r.code == "MEMORY_UNDO_UNAVAILABLE" && !r.retryable
    ));
    let status: String = sqlx::query_scalar("SELECT status FROM memory_items WHERE id=$1")
        .bind(Uuid::parse_str(saved["id"].as_str().unwrap())?)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(status, "deleted");
    owner.kill().await?;
    owner.wait().await?;
    h.close().await
}

/// Opens a WebSocket offering `protocols`; returns it and the protocol the server chose.
async fn connect_with(
    base: &str,
    user: &Value,
    protocols: &str,
) -> anyhow::Result<(Socket, Option<String>)> {
    let mut request = format!("{}/ws", base.replace("http:", "ws:")).into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", user["token"].as_str().unwrap()).parse()?,
    );
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", protocols.parse()?);
    let (socket, response) = tokio_tungstenite::connect_async(request).await?;
    let chosen = response
        .headers()
        .get("sec-websocket-protocol")
        .map(|p| p.to_str().unwrap().to_owned());
    Ok((socket, chosen))
}

fn hello(device: &str) -> Frame {
    frame(
        "",
        Body::Hello(Hello {
            protocol_major: 1,
            device_id: device.into(),
            ..Default::default()
        }),
    )
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS: scripts/test-integration.sh"]
async fn connections_compress_fall_back_to_events_and_ride_http2() -> anyhow::Result<()> {
    let h = Harness::start(|_| {}).await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let user_channel = format!("user:{org}:{}", id(&user, "user_id"));
    // A long title makes the user channel's snapshot worth compressing.
    let ctx = h.context(&user).await?;
    h.create(&ctx, &"压缩".repeat(40), "chat", json!({}), None)
        .await?;

    // zstd: preferred when offered; large frames come compressed, small ones plain.
    let (mut socket, chosen) = connect_with(
        &h.base,
        &user,
        &format!("{SUBPROTOCOL_ZSTD}, {SUBPROTOCOL}"),
    )
    .await?;
    assert_eq!(chosen.as_deref(), Some(SUBPROTOCOL_ZSTD));
    send(&mut socket, hello("zstd")).await?;
    subscribe(&mut socket, &user_channel, None).await?;
    let mut saw_compressed = false;
    let mut snapshot = None;
    for _ in 0..20 {
        let WsMessage::Binary(bytes) = socket.next().await.context("closed")?? else {
            continue;
        };
        saw_compressed |= bytes.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]);
        let f = Frame::decode(&*ryzhub_core::frame_bytes(&bytes).map_err(anyhow::Error::msg)?)?;
        if let Some(Body::Snapshot(s)) = f.body {
            snapshot = Some(s);
            break;
        }
    }
    assert!(saw_compressed, "the snapshot came compressed");
    assert!(snapshot.unwrap().items[0].text.starts_with("压缩"));

    // Server-sent events where WebSockets do not get through, frames posted back.
    let http = reqwest::Client::new();
    let mut events = http
        .get(format!("{}/stream?device_id=events", h.base))
        .bearer_auth(user["token"].as_str().unwrap())
        .send()
        .await?
        .error_for_status()?
        .bytes_stream();
    let mut buffer = String::new();
    let mut next_event = async || -> anyhow::Result<(String, String)> {
        loop {
            if let Some(end) = buffer.find("\n\n") {
                let block: String = buffer.drain(..end + 2).collect();
                let (mut kind, mut data) = (String::from("message"), String::new());
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        kind = v.trim().into();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim());
                    }
                }
                if !data.is_empty() {
                    return Ok((kind, data));
                }
                continue;
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), events.next())
                .await?
                .context("stream ended")??;
            buffer.push_str(std::str::from_utf8(&chunk)?);
        }
    };
    let (kind, connection) = next_event().await?;
    assert_eq!(kind, "connection");
    let decode = |data: &str| -> anyhow::Result<Frame> {
        use base64::Engine as _;
        Ok(Frame::decode(
            base64::engine::general_purpose::STANDARD
                .decode(data)?
                .as_slice(),
        )?)
    };
    let (_, welcome) = next_event().await?;
    assert!(
        matches!(decode(&welcome)?.body, Some(Body::Welcome(w)) if w.connection_id == connection)
    );
    let post = |f: Frame| {
        http.post(format!("{}/stream/{connection}", h.base))
            .bearer_auth(user["token"].as_str().unwrap())
            .body(f.encode_to_vec())
            .send()
    };
    post(frame(
        &user_channel,
        Body::Subscribe(Subscribe { since_seq: None }),
    ))
    .await?
    .error_for_status()?;
    let mut subscribed = false;
    for _ in 0..10 {
        let (_, data) = next_event().await?;
        if matches!(decode(&data)?.body, Some(Body::Snapshot(_))) {
            subscribed = true;
            break;
        }
    }
    assert!(subscribed, "the subscription answered over the stream");
    let other = h.user().await?;
    let stranger = http
        .post(format!("{}/stream/{connection}", h.base))
        .bearer_auth(other["token"].as_str().unwrap())
        .body(frame("", Body::Pong(Pong { nonce: 1 })).encode_to_vec())
        .send()
        .await?;
    assert_eq!(stranger.status(), 404, "a connection is only its user's");

    // RFC 8441: the WebSocket over an HTTP/2 extended CONNECT.
    let address = h.base.trim_start_matches("http://").to_owned();
    let io = hyper_util::rt::TokioIo::new(TcpStream::connect(&address).await?);
    let (mut sender, conn) =
        hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .handshake(io)
            .await?;
    tokio::spawn(conn);
    let request = hyper::Request::builder()
        .method(hyper::Method::CONNECT)
        .extension(hyper::ext::Protocol::from_static("websocket"))
        .uri(format!("http://{address}/ws"))
        .header("sec-websocket-version", "13")
        .header("sec-websocket-protocol", SUBPROTOCOL)
        .header(
            "authorization",
            format!("Bearer {}", user["token"].as_str().unwrap()),
        )
        .body(String::new())?;
    let mut response = sender.send_request(request).await?;
    assert_eq!(response.status(), 200);
    let upgraded = hyper::upgrade::on(&mut response).await?;
    let mut h2 = WebSocketStream::from_raw_socket(
        hyper_util::rt::TokioIo::new(upgraded),
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;
    h2.send(WsMessage::Binary(hello("h2").encode_to_vec().into()))
        .await?;
    let WsMessage::Binary(bytes) = h2.next().await.context("closed")?? else {
        anyhow::bail!("a binary Welcome")
    };
    assert!(matches!(Frame::decode(bytes)?.body, Some(Body::Welcome(_))));
    h.close().await
}

/// The member's API over plain HTTP: the response's status and body.
async fn api(
    h: &Harness,
    user: &Value,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> anyhow::Result<(reqwest::StatusCode, Value)> {
    let response = reqwest::Client::new()
        .request(
            method,
            format!("{}/orgs/{}{path}", h.base, id(user, "org_id")),
        )
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    Ok((status, response.json().await.unwrap_or(Value::Null)))
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn a_started_chat_keeps_its_provider_and_a_chosen_model_overrides_project_defaults()
-> anyhow::Result<()> {
    let route = |id: &str, protocol: &str| ModelRoute {
        id: id.into(),
        protocol: protocol.into(),
        harnesses: vec!["chat".into()],
        ..Default::default()
    };
    let h = Harness::start(|config| {
        config.llm.enabled = true;
        config.llm.models = vec![
            route("gpt-5.4", "responses"),
            route("claude-opus-4-6", "anthropic"),
        ];
    })
    .await?;
    let user = h.user().await?;
    let ctx = h.context(&user).await?;
    // Clients offer models by provider and send only the model.
    let (_, catalog) = api(&h, &user, reqwest::Method::GET, "/models", json!({})).await?;
    let provider = |id: &str| {
        let mut chat = catalog["chat"].as_array().into_iter().flatten();
        chat.find(|m| m["id"] == id).map(|m| m["provider"].clone())
    };
    assert_eq!(provider("gpt-5.4"), Some(json!("openai")), "{catalog}");
    assert_eq!(provider("claude-opus-4-6"), Some(json!("anthropic")));
    let model = |model: &str| json!({ "model": model });
    let conv = h
        .create(&ctx, "Model first", "chat", model("claude-opus-4-6"), None)
        .await?;
    assert_eq!(conv.run_config["protocol"], "anthropic");
    let conv = conv.id;
    let lease = h
        .app
        .store
        .claim(ctx.org_id, conv, Uuid::now_v7(), 10_000)
        .await?
        .unwrap();
    // The outcome of `command` once the Owner has processed it.
    let outcome = |command: Command| {
        let envelope = CommandEnvelope {
            cid: Uuid::now_v7().to_string(),
            command: Some(command),
        };
        let (h, ctx, lease) = (&h, &ctx, &lease);
        async move { processed(h, ctx, lease, &h.app.config, &envelope).await }
    };
    let message = SendMessage {
        text: "hello".into(),
        ..Default::default()
    };
    assert!(matches!(
        outcome(Command::SendMessage(message)).await?,
        Body::CommandAck(_)
    ));
    // Once it has run, another provider's model would need another history.
    let switch = |to: &str| {
        Command::UpdateRunConfig(UpdateRunConfig {
            config_json: model(to).to_string(),
        })
    };
    let refused = outcome(switch("gpt-5.4")).await?;
    assert!(
        matches!(&refused, Body::CommandRejected(r) if r.code == "HARNESS_PROTOCOL_IMMUTABLE"),
        "{refused:?}"
    );
    assert!(matches!(
        outcome(switch("fixture-model")).await?,
        Body::CommandAck(_)
    ));
    let stored: Value = sqlx::query_scalar("SELECT run_config FROM conversations WHERE id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(
        (&stored["model"], &stored["protocol"]),
        (&json!("fixture-model"), &json!("anthropic"))
    );
    // A project default's protocol does not fight the chosen model.
    let defaults = json!({"name":"Defaults",
        "defaults":{"mode":"chat","protocol":"responses","model":"gpt-5.4"}});
    let (_, project) = api(&h, &user, reqwest::Method::POST, "/projects", defaults).await?;
    let project = Some(id(&project, "id"));
    let chosen = h
        .create(&ctx, "Chosen", "chat", model("claude-opus-4-6"), project)
        .await?;
    assert_eq!(chosen.run_config["protocol"], "anthropic");
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn pro_mode_is_offered_where_the_model_has_it_and_reaches_its_requests() -> anyhow::Result<()>
{
    let h = Harness::start(|config| {
        config.llm.enabled = true;
        config.llm.models = vec![ModelRoute {
            id: "gpt-6.1-sol".into(),
            protocol: "responses".into(),
            harnesses: vec!["chat".into()],
            ..Default::default()
        }];
    })
    .await?;
    let user = h.user().await?;
    let (_, catalog) = api(&h, &user, reqwest::Method::GET, "/models", json!({})).await?;
    let pro = |id: &str| {
        let mut chat = catalog["chat"].as_array().into_iter().flatten();
        chat.find(|m| m["id"] == id).map(|m| m["pro"].clone())
    };
    assert_eq!(
        (pro("gpt-6.1-sol"), pro("fixture-model")),
        (Some(json!(true)), Some(json!(false))),
        "{catalog}"
    );
    let ctx = h.context(&user).await?;
    let settings = json!({"model":"gpt-6.1-sol","pro":true});
    let conv = h.create(&ctx, "Pro", "chat", settings, None).await?.id;
    let mut owner = h.owner()?;
    h.send(&user, conv, common::message("hello")).await?;
    h.wait_state(conv, "completed").await?;
    owner.kill().await?;
    let request = h.requests.lock().unwrap().last().unwrap().0.clone();
    assert_eq!(
        request["reasoning"],
        json!({"effort":"medium","mode":"pro"})
    );
    h.close().await
}

/// Wait until `count` sessions wait for a channel row lock.
async fn wait_for_channel_locks(db: &PgPool, count: i64) -> anyhow::Result<()> {
    for _ in 0..100 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database()
               AND wait_event_type='Lock' AND query LIKE 'SELECT id FROM channels%'",
        )
        .fetch_one(db)
        .await?;
        if waiting >= count {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("channel lock barrier not reached");
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn concurrent_project_delete_and_move_remain_recoverable() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let project = json!({"name":"Concurrent deletion"});
    let (_, project) = api(&h, &user, reqwest::Method::POST, "/projects", project).await?;
    let project = id(&project, "id");
    let ctx = h.context(&user).await?;
    let conv = h
        .create(&ctx, "并发删除", "chat", json!({}), Some(project))
        .await?
        .id;
    let mut blocker = h.db.begin().await?;
    sqlx::query("SELECT id FROM channels WHERE conversation_id=$1 FOR UPDATE")
        .bind(conv)
        .fetch_one(&mut *blocker)
        .await?;
    let client = reqwest::Client::new();
    let token = user["token"].as_str().unwrap();
    let deleting = tokio::spawn(
        client
            .delete(format!("{}/orgs/{org}/projects/{project}", h.base))
            .bearer_auth(token)
            .send(),
    );
    wait_for_channel_locks(&h.db, 1).await?;
    let moving = tokio::spawn(
        client
            .post(format!(
                "{}/orgs/{org}/conversations/{conv}/project",
                h.base
            ))
            .bearer_auth(token)
            .json(&json!({"project_id":null}))
            .send(),
    );
    wait_for_channel_locks(&h.db, 2).await?;
    blocker.commit().await?;
    assert!(deleting.await??.status().is_success());
    assert_eq!(moving.await??.status(), reqwest::StatusCode::FORBIDDEN);
    let row: (String, Option<Uuid>) =
        sqlx::query_as("SELECT state,project_id FROM conversations WHERE id=$1")
            .bind(conv)
            .fetch_one(&h.db)
            .await?;
    assert_eq!(row, ("deletion_pending".into(), Some(project)));
    let restore = format!("/projects/{project}/restore");
    let (status, _) = api(&h, &user, reqwest::Method::POST, &restore, json!({})).await?;
    assert!(status.is_success());
    let state: String = sqlx::query_scalar("SELECT state FROM conversations WHERE id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(state, "active", "restored with its project");
    let path = format!("/conversations/{conv}/project");
    let (status, _) = api(
        &h,
        &user,
        reqwest::Method::POST,
        &path,
        json!({"project_id":null}),
    )
    .await?;
    assert!(status.is_success());
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn conversation_settings_reach_online_offline_and_new_clients_as_one_event_each()
-> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let ctx = h.context(&user).await?;
    let (older, newer) = (h.conversation(&user).await?, h.conversation(&user).await?);
    let channel = format!("user:{}:{}", ctx.org_id, ctx.user_id);
    let client = || async {
        let mut client = Engine::default();
        client.subscribe(channel.clone());
        for frame in h.app.store.replay(&ctx, &channel, None).await?.0 {
            client.apply(frame);
        }
        anyhow::Ok(client)
    };
    let mut online = client().await?;
    let mut offline = online.clone();
    let settings = |conv: Uuid, body: Value| {
        let path = format!("/conversations/{conv}/settings");
        let (h, user) = (&h, &user);
        async move { api(h, user, reqwest::Method::PATCH, &path, body).await }
    };
    // Each change is one durable event, which the online client applies as it comes.
    let catch_up = |cursor: u64| {
        let (h, ctx, channel) = (&h, &ctx, &channel);
        async move {
            let (frames, _) = h.app.store.replay(ctx, channel, Some(cursor)).await?;
            ensure!(frames.len() == 1, "{} events for one change", frames.len());
            anyhow::Ok(frames)
        }
    };
    let item =
        |client: &Engine, conv: Uuid| client.channels[&channel].items[&conv.to_string()].clone();
    let pinned_at =
        |client: &Engine, conv: Uuid| match item(client, conv).detail.and_then(|d| d.kind) {
            Some(item_detail::Kind::Conversation(c)) => Ok(c.pinned_at_ms),
            other => Err(anyhow::anyhow!("no conversation detail: {other:?}")),
        };
    let mut revision = item(&online, newer).revision;
    for title in ["first rename", "second rename"] {
        settings(newer, json!({"title":title,"archive":true})).await?;
        for frame in catch_up(online.channels[&channel].cursor).await? {
            online.apply(frame);
        }
        let renamed = item(&online, newer);
        assert!(
            renamed.text == title && renamed.state == "archived" && renamed.revision > revision
        );
        revision = renamed.revision;
    }
    assert_eq!(pinned_at(&online, older)?, 0);
    settings(older, json!({"pinned":true})).await?;
    for frame in catch_up(online.channels[&channel].cursor).await? {
        online.apply(frame);
    }
    let first = pinned_at(&online, older)?;
    assert!(first > 0, "the online client sees the pin");
    // The list stays newest first and carries the pin; clients order pins themselves.
    let list = || async {
        let (_, list) = api(&h, &user, reqwest::Method::GET, "/conversations", json!({})).await?;
        anyhow::Ok(list.as_array().context("list")?.clone())
    };
    let listed = list().await?;
    assert_eq!(
        listed.iter().map(|c| id(c, "id")).collect::<Vec<_>>(),
        [newer, older]
    );
    assert!(listed[1]["pinned_at_ms"] == first && listed[0]["pinned_at_ms"].is_null());
    // A pin outlasts the list's limit.
    sqlx::query(
        "INSERT INTO conversations(id,org_id,creator_id,mode,title,created_at)
         SELECT gen_random_uuid(),$1,$2,'chat','filler',now()+n*interval '1 second'
         FROM generate_series(1,200) n",
    )
    .bind(ctx.org_id)
    .bind(ctx.user_id)
    .execute(&h.db)
    .await?;
    let listed = list().await?;
    assert!(listed.len() == 200 && listed[199]["id"] == older.to_string());
    sqlx::query("DELETE FROM conversations WHERE org_id=$1 AND title='filler'")
        .bind(ctx.org_id)
        .execute(&h.db)
        .await?;
    // A client starting afresh reads it all from the snapshot.
    let fresh = client().await?;
    assert_eq!(pinned_at(&fresh, older)?, first);
    assert_eq!(item(&fresh, newer).text, "second rename");
    // Pinning again keeps the original time.
    settings(older, json!({"pinned":true})).await?;
    for frame in catch_up(online.channels[&channel].cursor).await? {
        online.apply(frame);
    }
    assert_eq!(pinned_at(&online, older)?, first);
    settings(older, json!({"pinned":false})).await?;
    for frame in catch_up(online.channels[&channel].cursor).await? {
        online.apply(frame);
    }
    assert_eq!(pinned_at(&online, older)?, 0);
    assert!(list().await?[1]["pinned_at_ms"].is_null());
    // A client that was away replays every change and ends where the online one is.
    let (frames, _) = h
        .app
        .store
        .replay(&ctx, &channel, Some(offline.channels[&channel].cursor))
        .await?;
    assert_eq!(frames.len(), 5);
    for frame in frames {
        offline.apply(frame);
    }
    assert_eq!(
        offline.channels[&channel].items,
        online.channels[&channel].items
    );
    // A temporary conversation cannot be pinned.
    let temporary = h
        .create(&ctx, "临时", "chat", json!({"temporary":true}), None)
        .await?;
    let (status, refused) = settings(temporary.id, json!({"pinned":true})).await?;
    assert_eq!(
        (status.as_u16(), &refused["error"]),
        (400, &json!("INVALID_CONVERSATION"))
    );
    h.close().await
}
