//! Requires the local PostgreSQL 18 + NATS services: scripts/test-integration.sh.
use anyhow::{Context, ensure};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use ryzhub_core::Engine;
use ryzhub_proto::{command_envelope::Command, event_envelope::Event, frame::Body, *};
use ryzhub_server::{
    app::App,
    config::Config,
    http::router,
    owner::{self, OwnerLink},
    store::{Store, append},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{process::Stdio, time::Duration};
use tokio::{
    net::TcpStream,
    process::{Child, Command as Process},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message as WsMessage, client::IntoClientRequest},
};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
struct Harness {
    name: String,
    admin: PgPool,
    db: PgPool,
    store: Store,
    nats: async_nats::Client,
    app_url: String,
    worker_url: String,
    base: String,
    task: tokio::task::JoinHandle<()>,
}
impl Harness {
    async fn new() -> anyhow::Result<Self> {
        let admin_url = std::env::var("TEST_ADMIN_URL").unwrap_or_else(|_| {
            "postgres://postgres:local-development-only@127.0.0.1:55432/postgres".into()
        });
        let admin = PgPool::connect(&admin_url).await?;
        let name = format!("ryzhub_test_{}", Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await?;
        let root = admin_url.rsplit_once('/').unwrap().0;
        let db = PgPool::connect(&format!("{root}/{name}")).await?;
        sqlx::migrate!("../../migrations").run(&db).await?;
        let address = root.split('@').nth(1).context("test URL")?;
        let app_url = format!("postgres://ryzhub_app:local-app-only@{address}/{name}");
        let worker_url = format!("postgres://ryzhub_worker:local-worker-only@{address}/{name}");
        let store = Store::connect(&app_url).await?;
        let nats = async_nats::connect("nats://127.0.0.1:54222").await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let app = App::new(store.clone(), nats.clone(), Config::default())?;
        let task = tokio::spawn(async move {
            axum::serve(listener, router(app)).await.unwrap();
        });
        Ok(Self {
            name,
            admin,
            db,
            store,
            nats,
            app_url,
            worker_url,
            base,
            task,
        })
    }
    async fn user(&self) -> anyhow::Result<Value> {
        Ok(reqwest::Client::new().post(format!("{}/auth/register",self.base)).json(&json!({"email":format!("{}@example.test",Uuid::now_v7()),"password":"correct-test-password"})).send().await?.error_for_status()?.json().await?)
    }
    async fn conversation(&self, user: &Value) -> anyhow::Result<Uuid> {
        let response: Value = reqwest::Client::new()
            .post(format!(
                "{}/orgs/{}/conversations",
                self.base,
                user["org_id"].as_str().unwrap()
            ))
            .bearer_auth(user["token"].as_str().unwrap())
            .json(&json!({"title":"一致性验收","mode":"chat"}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(Uuid::parse_str(response["id"].as_str().unwrap())?)
    }
    fn owner(&self) -> anyhow::Result<Child> {
        Ok(Process::new(env!("CARGO_BIN_EXE_ryzhub-server"))
            .arg("owner")
            .env("DATABASE_URL", &self.app_url)
            .env("WORKER_DATABASE_URL", &self.worker_url)
            .env("NATS_URL", "nats://127.0.0.1:54222")
            .env("RUST_LOG", "warn")
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?)
    }
    async fn close(self) -> anyhow::Result<()> {
        self.task.abort();
        let _ = self.task.await;
        self.store.pool.close().await;
        self.db.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await?;
        self.admin.close().await;
        Ok(())
    }
}
fn id(user: &Value, key: &str) -> Uuid {
    Uuid::parse_str(user[key].as_str().unwrap()).unwrap()
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
async fn subscribe(socket: &mut Socket, channel: &str, since: Option<u64>) -> anyhow::Result<()> {
    send(
        socket,
        frame(channel, Body::Subscribe(Subscribe { since_seq: since })),
    )
    .await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn database_idempotency_fencing_rls_and_transaction_outbox() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let other = h.user().await?;
    let conv = h.conversation(&user).await?;
    let ctx = h
        .store
        .context(id(&user, "user_id"), id(&user, "org_id"))
        .await?;
    let channel = format!("conv:{conv}");
    let cid = Uuid::now_v7();
    let cmd = message(cid, "atomic transaction");
    assert!(
        h.store
            .accept(&ctx, conv, "device-a", &cmd)
            .await?
            .is_none()
    );
    assert!(
        h.store
            .accept(&ctx, conv, "device-b", &cmd)
            .await?
            .is_none()
    );
    let old = h
        .store
        .claim(ctx.org_id, conv, Uuid::now_v7(), 3000)
        .await?
        .unwrap();
    assert!(owner::process_command(&h.store, &old).await?);
    let first = h.store.command_result(&ctx, cid).await?.unwrap();
    assert_eq!(
        h.store.accept(&ctx, conv, "device-b", &cmd).await?.unwrap(),
        first
    );
    assert!(
        h.store
            .accept(&ctx, conv, "device-a", &message(cid, "different payload"))
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM commands WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&h.db)
        .await?;
    assert_eq!(count, 1);
    let mut tx = h.store.tx(id(&other, "org_id")).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM items WHERE conversation_id=$1")
        .bind(conv)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(count, 0);
    assert!(sqlx::query("INSERT INTO runs(id,org_id,conversation_id,exec_epoch,state,response_text,assistant_item_id,cause_cid) VALUES($1,$2,$3,1,'running','x',$4,$5)").bind(Uuid::now_v7()).bind(id(&other,"org_id")).bind(conv).bind(Uuid::now_v7()).bind(Uuid::now_v7()).execute(&mut *tx).await.is_err());
    tx.rollback().await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM items")
            .fetch_one(&h.store.pool)
            .await?,
        0
    );
    assert!(
        h.store
            .authorize_channel(id(&other, "user_id"), &channel)
            .await
            .is_err()
    );
    let pending = h.store.unpublished(ctx.org_id, &channel).await?;
    assert_eq!(pending.len(), 3);
    // Process crashes after commit and before publishing: force only its lease to expire.
    sqlx::query("UPDATE channels SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(&channel)
        .execute(&h.db)
        .await?;
    let new = h
        .store
        .claim(ctx.org_id, conv, Uuid::now_v7(), 3000)
        .await?
        .unwrap();
    assert!(new.epoch > old.epoch);
    assert!(h.store.renew(&old, 3000).await.is_err());
    let mut tx = h.store.tx(ctx.org_id).await?;
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
        owner::publish(&h.store, &h.nats, ctx.org_id, &channel, Some(&old), vec![])
            .await
            .is_err()
    );
    let mut subscriber = h.nats.subscribe(owner::subject(&channel)).await?;
    h.nats.flush().await?;
    owner::publish(&h.store, &h.nats, ctx.org_id, &channel, Some(&new), vec![]).await?;
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
    assert!(h.store.unpublished(ctx.org_id, &channel).await?.is_empty());
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18 and NATS"]
async fn two_devices_approval_cas_and_stale_execution_epoch() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let ctx = h
        .store
        .context(id(&user, "user_id"), id(&user, "org_id"))
        .await?;
    h.store
        .accept(&ctx, conv, "a", &message(Uuid::now_v7(), "/approve test"))
        .await?;
    let lease = h
        .store
        .claim(ctx.org_id, conv, Uuid::now_v7(), 10_000)
        .await?
        .unwrap();
    owner::process_command(&h.store, &lease).await?;
    let (action, epoch): (Uuid, i64) =
        sqlx::query_as("SELECT id,run_epoch FROM pending_actions WHERE org_id=$1")
            .bind(ctx.org_id)
            .fetch_one(&h.db)
            .await?;
    // A takeover keeps the request visible but invalidates the old execution epoch.
    sqlx::query(
        "UPDATE channels SET lease_until=now()-interval '1 second' WHERE conversation_id=$1",
    )
    .bind(conv)
    .execute(&h.db)
    .await?;
    let lease = h
        .store
        .claim(ctx.org_id, conv, Uuid::now_v7(), 10_000)
        .await?
        .unwrap();
    let stale = CommandEnvelope {
        cid: Uuid::now_v7().to_string(),
        command: Some(Command::DecideApproval(DecideApproval {
            action_id: action.to_string(),
            run_epoch: epoch as u64,
            decision: "allow".into(),
        })),
    };
    h.store.accept(&ctx, conv, "old-device", &stale).await?;
    owner::process_command(&h.store, &lease).await?;
    assert!(matches!(
        h.store
            .command_result(&ctx, Uuid::parse_str(&stale.cid)?)
            .await?
            .unwrap()
            .body,
        Some(Body::CommandRejected(_))
    ));
    let epoch = lease.epoch;
    let a = CommandEnvelope {
        cid: Uuid::now_v7().to_string(),
        command: Some(Command::DecideApproval(DecideApproval {
            action_id: action.to_string(),
            run_epoch: epoch as u64,
            decision: "allow".into(),
        })),
    };
    let b = CommandEnvelope {
        cid: Uuid::now_v7().to_string(),
        ..a.clone()
    };
    let (ra, rb) = tokio::join!(
        h.store.accept(&ctx, conv, "a", &a),
        h.store.accept(&ctx, conv, "b", &b)
    );
    ra?;
    rb?;
    owner::process_command(&h.store, &lease).await?;
    owner::process_command(&h.store, &lease).await?;
    let mut accepted = 0;
    let mut rejected = 0;
    for cmd in [a, b] {
        match h
            .store
            .command_result(&ctx, Uuid::parse_str(&cmd.cid)?)
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
            Body::Command(message(cid, &"切网与接管 ".repeat(35))),
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
            Body::Command(message(cid, &"切网与接管 ".repeat(35))),
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
    assert!(final_text.ends_with("Rust 🦀。"));
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
async fn gateway_process_restart_client_resubscribes_and_revocation_clears_it() -> anyhow::Result<()>
{
    let h = Harness::new().await?;
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let channel = format!("conv:{conv}");
    let reserve = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = reserve.local_addr()?;
    drop(reserve);
    let config_path = std::env::temp_dir().join(format!("ryzhub-gateway-{}.toml", Uuid::now_v7()));
    std::fs::write(&config_path, format!("listen = \"{addr}\"\n"))?;
    let spawn = || {
        Process::new(env!("CARGO_BIN_EXE_ryzhub-server"))
            .arg("gateway")
            .env("DATABASE_URL", &h.app_url)
            .env("NATS_URL", "nats://127.0.0.1:54222")
            .env("RYZHUB_CONFIG", &config_path)
            .kill_on_drop(true)
            .spawn()
    };
    let base = format!("http://{addr}");
    let mut child = spawn()?;
    let ready = || async {
        for _ in 0..100 {
            if reqwest::get(format!("{base}/healthz")).await.is_ok() {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        anyhow::bail!("gateway startup timeout")
    };
    ready().await?;
    let mut request = format!("ws://{addr}/ws").into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", user["token"].as_str().unwrap()).parse()?,
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request.clone()).await?;
    send(
        &mut socket,
        frame(
            "",
            Body::Hello(Hello {
                protocol_major: 1,
                device_id: "stable-device".into(),
                ..Default::default()
            }),
        ),
    )
    .await?;
    assert!(matches!(
        next(&mut socket).await?.body,
        Some(Body::Welcome(_))
    ));
    subscribe(&mut socket, &channel, None).await?;
    until(&mut socket, |f| {
        matches!(f.body, Some(Body::SubscriptionSynced(_)))
    })
    .await?;
    child.kill().await?;
    child.wait().await?;
    let mut child = spawn()?;
    ready().await?;
    let (mut restored, _) = tokio_tungstenite::connect_async(request).await?;
    send(
        &mut restored,
        frame(
            "",
            Body::Hello(Hello {
                protocol_major: 1,
                device_id: "stable-device".into(),
                ..Default::default()
            }),
        ),
    )
    .await?;
    assert!(matches!(
        next(&mut restored).await?.body,
        Some(Body::Welcome(_))
    ));
    // Subscriptions live in the client, which resubscribes from its own cursor.
    subscribe(&mut restored, &channel, None).await?;
    let synced = until(&mut restored, |f| {
        matches!(f.body, Some(Body::SubscriptionSynced(_)))
    })
    .await?;
    assert_eq!(synced.channel, channel);
    reqwest::Client::new()
        .post(format!("{base}/auth/logout"))
        .bearer_auth(user["token"].as_str().unwrap())
        .send()
        .await?
        .error_for_status()?;
    assert!(matches!(
        next(&mut restored).await?.body,
        Some(Body::SubscriptionRevoked(_))
    ));
    child.kill().await?;
    child.wait().await?;
    std::fs::remove_file(config_path)?;
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
    let ctx = h.store.context(user, org).await?;
    let conv = h
        .store
        .create_conversation(&ctx, "待审批任务", "work")
        .await?
        .id;
    let channel = format!("conv:{conv}");
    let mut owner = h.owner()?;
    let mut socket = connect(&h.base, &member, "member-device").await?;
    subscribe(&mut socket, &channel, None).await?;
    send(
        &mut socket,
        frame(
            &channel,
            Body::Command(message(Uuid::now_v7(), "/approve member removal")),
        ),
    )
    .await?;
    until(&mut socket,|f|matches!(&f.body,Some(Body::Event(e)) if matches!(e.event,Some(Event::PendingActionChanged(_))))).await?;
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
    // The session survives for the member's other organizations; this one is gone.
    h.store
        .authenticate(member["token"].as_str().unwrap())
        .await?;
    assert!(h.store.context(user, org).await.is_err());
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
        .store
        .claim(org, conv, Uuid::now_v7(), 3000)
        .await?
        .unwrap();
    sqlx::query("UPDATE channels SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(&channel)
        .execute(&h.db)
        .await?;
    let current = h
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
        h.nats
            .publish(owner::subject(&channel), f.encode_to_vec().into())
            .await?;
    }
    h.nats.flush().await?;
    let received = next(&mut socket).await?;
    assert!(matches!(received.body,Some(Body::Delta(d)) if d.text=="current publisher"));
    socket.close(None).await?;
    h.close().await
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn parallel_approvals_should_both_resolve() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let outcome = async {
        let user = h.user().await?;
        let org = id(&user, "org_id");
        let conv = h.conversation(&user).await?;
        let ctx = h.store.context(id(&user, "user_id"), org).await?;
        let lease = h.store.claim(org, conv, Uuid::now_v7(), 60000).await?.unwrap();
        let mut config = Config::default();
        config.llm.enabled = true;
        config.llm.model = "mock".into();
        h.store.accept(&ctx, conv, "review", &message(Uuid::now_v7(), "parallel approval")).await?;
        owner::process_with_config(&h.store, &lease, &config).await?;
        let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1").bind(conv).fetch_one(&h.db).await?;
        let actions = [Uuid::now_v7(), Uuid::now_v7()];
        for action in actions {
            sqlx::query("INSERT INTO pending_actions(id,org_id,run_id,run_epoch,kind,summary,options,expires_at) VALUES($1,$2,$3,$4,'approval','parallel MCP approval',ARRAY['allow','deny'],now()+interval '10 minutes')")
                .bind(action).bind(org).bind(run).bind(lease.epoch).execute(&h.db).await?;
        }
        sqlx::query("UPDATE runs SET state='requires_action' WHERE id=$1").bind(run).execute(&h.db).await?;
        let mut replies = vec![];
        for (index, action) in actions.into_iter().enumerate() {
            let cmd = CommandEnvelope { cid: Uuid::now_v7().to_string(), command: Some(Command::DecideApproval(DecideApproval { action_id: action.to_string(),run_epoch:lease.epoch as u64,decision:"allow".into() })) };
            h.store.accept(&ctx,conv,"review",&cmd).await?;
            owner::process_with_config(&h.store,&lease,&config).await?;
            let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=$1").bind(run).fetch_one(&h.db).await?;
            ensure!(state == if index == 0 { "requires_action" } else { "running" });
            replies.push(h.store.command_result(&ctx,Uuid::parse_str(&cmd.cid)?).await?.unwrap());
        }
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=$1").bind(run).fetch_one(&h.db).await?;
        let pending:i64 = sqlx::query_scalar("SELECT count(*) FROM pending_actions WHERE run_id=$1 AND status='pending'").bind(run).fetch_one(&h.db).await?;
        println!("parallel approvals: responses={replies:?}, run={state}, pending={pending}");
        ensure!(replies.iter().all(|f|matches!(f.body,Some(Body::CommandAck(_)))), "second approval was rejected while still pending");
        Ok::<_,anyhow::Error>(())
    }.await;
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn restore_failure_should_not_create_blank_workspace() -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let h = Harness::new().await?;
    let temp = tempfile::tempdir()?;
    let socket = temp.path().join("host.sock");
    let listener = tokio::net::UnixListener::bind(&socket)?;
    let created = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = created.clone();
    let stopped = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let stops = stopped.clone();
    let mut restored = false;
    let stub = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut stream) = stream.into_split();
            let mut reader = tokio::io::BufReader::new(read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let command: Value = serde_json::from_str(&line).unwrap();
            if command["kind"] == "artifact_upload" {
                let mut body = vec![0; command["size"].as_u64().unwrap() as usize];
                tokio::io::AsyncReadExt::read_exact(&mut reader, &mut body)
                    .await
                    .unwrap();
            }
            if command["kind"] == "stop" {
                stops
                    .lock()
                    .unwrap()
                    .push(command["sandbox_id"].as_str().unwrap().to_owned());
            }
            let reply = if command["kind"] == "restore" && !restored {
                restored = true;
                // Destination was created, but the Owner lost its lease before DB completion.
                json!({"ok":false,"value":null,"error":"FENCED"})
            } else if command["kind"] == "create" {
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                json!({"ok":false,"value":null,"error":"REVIEW_STOP_BEFORE_CREATE"})
            } else {
                json!({"ok":true,"value":{"stopped":true},"error":null})
            };
            stream
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    let outcome = async {
        let user=h.user().await?;
        let org=id(&user,"org_id");
        let conv=h.conversation(&user).await?;
        sqlx::query("UPDATE conversations SET mode='work',run_config='{}' WHERE id=$1").bind(conv).execute(&h.db).await?;
        let ctx=h.store.context(id(&user,"user_id"),org).await?;
        let lease=h.store.claim(org,conv,Uuid::now_v7(),60000).await?.unwrap();
        let mut config=Config::default();
        config.llm.enabled=true;
        config.work.enabled=true;
        config.work.model="mock".into();
        config.sandbox.enabled=true;
        config.sandbox.host_socket=socket.to_str().unwrap().into();
        config.sandbox.image=format!("review@sha256:{}","b".repeat(64));
        config.work.storage_root=temp.path().join("missing-blobs").to_str().unwrap().into();
        config.work.secondary_root=None;
        h.store.accept(&ctx,conv,"review",&message(Uuid::now_v7(),"first turn")).await?;
        owner::process_with_config(&h.store,&lease,&config).await?;
        let run:Uuid=sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1").bind(conv).fetch_one(&h.db).await?;
        sqlx::query("UPDATE runs SET state='completed',ended_at=now() WHERE id=$1").bind(run).execute(&h.db).await?;
        let sandbox=Uuid::now_v7();
        sqlx::query("INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent,native_thread_id,node_lease_until) VALUES($1,$2,$3,$4,1,$5,'running','{}',true,'original-thread',now()+interval '15 seconds')")
            .bind(sandbox).bind(org).bind(conv).bind(run).bind(lease.epoch).execute(&h.db).await?;
        sqlx::query("UPDATE runs SET sandbox_id=$2 WHERE id=$1").bind(run).bind(sandbox).execute(&h.db).await?;
        let checkpoint=Uuid::now_v7();
        use sha2::Digest;
        let bytes=b"checkpoint fixture";
        let checksum=format!("{:x}",sha2::Sha256::digest(bytes));
        let manifest=json!({"image":config.sandbox.image,"format":1,"encoding":"tar.zst","codex_version":"0.154.0","sha256":checksum,"thread_id":"original-thread"});
        sqlx::query("INSERT INTO checkpoints(id,org_id,conversation_id,sandbox_id,manifest,channel_seq,gseq) VALUES($1,$2,$3,$4,$5,0,0)")
            .bind(checkpoint).bind(org).bind(conv).bind(sandbox).bind(manifest).execute(&h.db).await?;
        sqlx::query("INSERT INTO workspace_heads(org_id,conversation_id,checkpoint_id) VALUES($1,$2,$3)").bind(org).bind(conv).bind(checkpoint).execute(&h.db).await?;
        let job=Uuid::now_v7();
        sqlx::query("INSERT INTO workspace_jobs(id,org_id,conversation_id,kind) VALUES($1,$2,$3,'restore')").bind(job).bind(org).bind(conv).execute(&h.db).await?;
        let app=App::new(h.store.clone(),h.nats.clone(),config.clone())?;
        let link=OwnerLink::detached();
        ryzhub_server::workspace::jobs(&app,&lease,&link).await?;
        let state:String=sqlx::query_scalar("SELECT state FROM workspace_jobs WHERE id=$1").bind(job).fetch_one(&h.db).await?;
        ensure!(state=="failed","expected unavailable blob to fail restore");
        let cmd=message(Uuid::now_v7(),"continue after failed restore");
        h.store.accept(&ctx,conv,"review",&cmd).await?;
        owner::process_with_config(&h.store,&lease,&config).await?;
        let response=h.store.command_result(&ctx,Uuid::parse_str(&cmd.cid)?).await?.unwrap();
        if matches!(response.body,Some(Body::CommandAck(_))) {
            let next:Uuid=sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1 AND state='running'").bind(conv).fetch_one(&h.db).await?;
            let err=ryzhub_server::work::ensure_work(&app,&lease,next).await.unwrap_err();
            println!("restore job={state}; next message={response:?}; ensure_work={err}; create_sent={}",created.load(std::sync::atomic::Ordering::SeqCst));
        }
        ensure!(matches!(response.body, Some(Body::CommandRejected(ref r)) if r.code == "WORKSPACE_RECOVERY_REQUIRED"), "message must fail before creating a run");
        ensure!(!created.load(std::sync::atomic::Ordering::SeqCst),"failed restore allowed creation of a new blank workspace");
        let mut headers=axum::http::HeaderMap::new();
        headers.insert("Authorization",format!("Bearer {}",user["token"].as_str().unwrap()).parse()?);
        let retried=ryzhub_server::workspace::enqueue(axum::extract::State(app.clone()),headers,
            axum::extract::Path((org,conv)),axum::Json(ryzhub_server::workspace::JobInput{kind:"restore".into()})).await.map_err(|e| e.0)?;
        ensure!(retried.0["id"]==job.to_string(), "retry must reuse restore destination");
        ryzhub_server::workspace::blob::BlobStore::configured(&config.work.storage_root)?.scoped(org).put(&checksum,bytes).await?;
        let interrupted=ryzhub_server::workspace::jobs(&app,&lease,&link).await.unwrap_err();
        ensure!(ryzhub_server::error::is_fenced(&interrupted), "expected FENCED, got {interrupted:#}");
        sqlx::query("UPDATE channels SET lease_until=now()-interval '1 second' WHERE org_id=$1 AND conversation_id=$2").bind(org).bind(conv).execute(&h.db).await?;
        let successor=h.store.claim(org,conv,Uuid::now_v7(),60000).await?.unwrap();
        ryzhub_server::workspace::jobs(&app,&successor,&link).await?;
        let state:String=sqlx::query_scalar("SELECT state FROM workspace_jobs WHERE id=$1").bind(job).fetch_one(&h.db).await?;
        ensure!(state=="completed");
        ensure!(stopped.lock().unwrap().iter().all(|id| id==&sandbox.to_string()), "retry stopped destination");
        let generation:i64=sqlx::query_scalar("SELECT generation FROM workspace_heads WHERE conversation_id=$1").bind(conv).fetch_one(&h.db).await?;
        ensure!(generation==2, "retry incremented generation again");
        let seq:i64=sqlx::query_scalar("SELECT last_seq FROM channels WHERE org_id=$1 AND conversation_id=$2").bind(org).bind(conv).fetch_one(&h.db).await?;
        // A crash after applying but before completing the job must not append again.
        ryzhub_server::workspace::restore(&app,&successor,job,&link).await?;
        let again:i64=sqlx::query_scalar("SELECT last_seq FROM channels WHERE org_id=$1 AND conversation_id=$2").bind(org).bind(conv).fetch_one(&h.db).await?;
        ensure!(seq==again);
        let mut tx=h.store.tx(org).await?;
        ryzhub_server::work::available(&mut tx,&successor).await?;
        tx.commit().await?;
        Ok::<_,anyhow::Error>(())
    }.await;
    stub.abort();
    h.close().await?;
    outcome
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn checkpoint_sync_should_not_ack_next_turn_without_projection() -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let h = Harness::new().await?;
    let temp = tempfile::tempdir()?;
    let socket = temp.path().join("host.sock");
    let listener = tokio::net::UnixListener::bind(&socket)?;
    let tokens = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let observed = tokens.clone();
    let stub = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut line = String::new();
            tokio::io::BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let command = &request["request"]["command"];
            if command["kind"] == "rotate_key" {
                observed
                    .lock()
                    .unwrap()
                    .push(command["token"].as_str().unwrap().to_owned());
            }
            let value = if command["kind"] == "replay" {
                let events = vec![
                    json!({"gseq":1,"op_id":"native","result":{"type":"item","phase":"completed","turn_id":"new-turn","native_id":"new-native-item","kind":"assistant","content":{"text":"NEW_RUN_CONTENT"}}}),
                    json!({"gseq":2,"op_id":"native","result":{"type":"question_requested","request_id":"17","turn_id":"new-turn","questions":[{"id":"color","question":"Which color?"}]}}),
                    json!({"gseq":3,"op_id":"native","result":{"type":"turn_started","turn_id":"restored-historical-turn"}}),
                ];
                let events: Vec<_> = events
                    .into_iter()
                    .filter(|e| e["gseq"].as_u64().unwrap() > command["after"].as_u64().unwrap())
                    .collect();
                json!({"events":events,"head":3,"folds":{}})
            } else {
                json!({"acked":1})
            };
            stream
                .write_all(
                    format!("{}\n", json!({"ok":true,"value":value,"error":null})).as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let outcome=async {
        let user=h.user().await?;
        let org=id(&user,"org_id");
        let conv=h.conversation(&user).await?;
        let ctx=h.store.context(id(&user,"user_id"),org).await?;
        let lease=h.store.claim(org,conv,Uuid::now_v7(),60000).await?.unwrap();
        let mut config=Config::default();
        config.llm.enabled=true;
        config.llm.model="mock".into();
        config.llm.unix_socket=Some("/gateway/test.sock".into());
        config.work.model="mock".into();
        config.sandbox.enabled=true;
        config.sandbox.host_socket=socket.to_str().unwrap().into();
        h.store.accept(&ctx,conv,"review",&message(Uuid::now_v7(),"old turn")).await?;
        owner::process_with_config(&h.store,&lease,&config).await?;
        let old:Uuid=sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1").bind(conv).fetch_one(&h.db).await?;
        sqlx::query("UPDATE runs SET state='completed',native_turn_id='old-turn',ended_at=now() WHERE id=$1").bind(old).execute(&h.db).await?;
        h.store.accept(&ctx,conv,"review",&message(Uuid::now_v7(),"new turn")).await?;
        owner::process_with_config(&h.store,&lease,&config).await?;
        let new:Uuid=sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1 AND state='running'").bind(conv).fetch_one(&h.db).await?;

        let sandbox=Uuid::now_v7();
        sqlx::query("INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent) VALUES($1,$2,$3,$4,1,$5,'running','{}',true)")
            .bind(sandbox).bind(org).bind(conv).bind(new).bind(lease.epoch).execute(&h.db).await?;
        sqlx::query("UPDATE runs SET sandbox_id=$2 WHERE id=ANY($1)").bind(vec![old,new]).bind(sandbox).execute(&h.db).await?;
        let app=App::new(h.store.clone(),h.nats.clone(),config)?;
        let link=OwnerLink::detached();
        // The event's turn is not registered yet: projection waits instead of acknowledging it.
        let waiting=tokio::time::timeout(Duration::from_millis(500),ryzhub_server::work::sync_through(&app,&lease,old,sandbox,1,&link)).await;
        ensure!(waiting.is_err(), "sync must wait for the turn to be registered");
        let deferred:i64=sqlx::query_scalar("SELECT acked_gseq FROM sandboxes WHERE id=$1").bind(sandbox).fetch_one(&h.db).await?;
        ensure!(deferred == 0, "unregistered turn must not be acknowledged");
        sqlx::query("UPDATE runs SET native_turn_id='new-turn' WHERE id=$1").bind(new).execute(&h.db).await?;
        ryzhub_server::work::sync_through(&app,&lease,old,sandbox,1,&link).await?;
        ryzhub_server::work::sync_through(&app,&lease,new,sandbox,3,&link).await?;
        let ack:i64=sqlx::query_scalar("SELECT acked_gseq FROM sandboxes WHERE id=$1").bind(sandbox).fetch_one(&h.db).await?;
        let projected:i64=sqlx::query_scalar("SELECT count(*) FROM items WHERE run_id=$1 AND text='NEW_RUN_CONTENT'").bind(new).fetch_one(&h.db).await?;
        println!("checkpoint sync: acked_gseq={ack}, new-turn projected items={projected}");
        ensure!(ack==3, "historical usage notification must not block replay after restore");
        ensure!(projected==1,"checkpoint consumed and acknowledged a newer turn event without projecting it");
        let (action,options):(Uuid,Vec<String>)=sqlx::query_as("SELECT id,options FROM pending_actions WHERE run_id=$1").bind(new).fetch_one(&h.db).await?;
        ensure!(options==vec!["__answer__"], "Work question must render an answer field");
        // Rotation also works while waiting for user input, without starting a new turn, and
        // supersedes the previous key for new requests.
        ryzhub_server::work::refresh_key(&app,&lease,new,sandbox).await?;
        let first=tokens.lock().unwrap().last().unwrap().clone();
        ryzhub_server::work::refresh_key(&app,&lease,new,sandbox).await?;
        let second=tokens.lock().unwrap().last().unwrap().clone();
        ensure!(first != second);
        let first=ryzhub_server::llm::keys::resolve(&h.store,&first).await?;
        let second=ryzhub_server::llm::keys::resolve(&h.store,&second).await?;
        let mut tx=h.store.tx(org).await?;
        ensure!(ryzhub_server::llm::gateway::validate_key(&mut tx,&first,false).await.is_err());
        tx.rollback().await?;
        let mut tx=h.store.tx(org).await?;
        ryzhub_server::llm::gateway::validate_key(&mut tx,&second,false).await?;
        // An accepted stream outlives its key's supersession.
        ryzhub_server::llm::gateway::validate_key(&mut tx,&first,true).await?;
        tx.commit().await?;
        let answer=CommandEnvelope{cid:Uuid::now_v7().to_string(),command:Some(Command::AnswerQuestion(AnswerQuestion{action_id:action.to_string(),run_epoch:lease.epoch as u64,answer:"green".into()}))};
        h.store.accept(&ctx,conv,"review",&answer).await?;
        owner::process_with_config(&h.store,&lease,&app.config).await?;
        let response=h.store.command_result(&ctx,Uuid::parse_str(&answer.cid)?).await?.unwrap();
        ensure!(matches!(response.body,Some(Body::CommandAck(_))));
        let answer:Value=sqlx::query_scalar("SELECT answer FROM native_requests WHERE action_id=$1").bind(action).fetch_one(&h.db).await?;
        ensure!(answer==json!({"type":"question","answers":{"color":"green"}}), "unexpected answer {answer}");
        Ok::<_,anyhow::Error>(())
    }.await;
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
        let org = id(&user,"org_id");
        let ctx = h.store.context(id(&user,"user_id"),org).await?;
        for mode in ["chat","work"] {
            let conv = h.store.create_conversation(&ctx,"Legacy upgrade",mode).await?.id;
            let run=Uuid::now_v7();
            sqlx::query("INSERT INTO runs(id,org_id,conversation_id,harness,model,exec_epoch,state,response_text,assistant_item_id,cause_cid) VALUES($1,$2,$3,$4,'pinned-legacy-model',1,'running','',$5,$6)")
                .bind(run).bind(org).bind(conv).bind(if mode=="work" {"codex"}else{"chat"}).bind(Uuid::now_v7()).bind(Uuid::now_v7()).execute(&h.db).await?;
            sqlx::raw_sql(include_str!("../../../migrations/0011_legacy_run_settings.sql")).execute(&h.db).await?;
            let (protocol,settings):(String,Value)=sqlx::query_as("SELECT protocol,run_config FROM runs WHERE id=$1").bind(run).fetch_one(&h.db).await?;
            ensure!(settings["model"]=="pinned-legacy-model");
            ensure!(protocol==if mode=="work" {"responses"}else{"anthropic"});
            let stored:Value=sqlx::query_scalar("SELECT run_config FROM conversations WHERE id=$1").bind(conv).fetch_one(&h.db).await?;
            ensure!(stored==settings);
            sqlx::query("UPDATE conversations SET run_config=jsonb_set(run_config,'{reasoning}','\"high\"') WHERE id=$1").bind(conv).execute(&h.db).await?;
            sqlx::raw_sql(include_str!("../../../migrations/0011_legacy_run_settings.sql")).execute(&h.db).await?;
            let reasoning:String=sqlx::query_scalar("SELECT run_config->>'reasoning' FROM conversations WHERE id=$1").bind(conv).fetch_one(&h.db).await?;
            ensure!(reasoning=="high","backfill cannot overwrite configured conversations");
        }
        Ok::<_,anyhow::Error>(())
    }.await;
    h.close().await?;
    outcome
}

/// Every request a stub host received.
type Recorded = std::sync::Arc<std::sync::Mutex<Vec<Value>>>;

/// A host agent stub that records every request and answers each as a completed operation
/// (with an empty model list for harness requests).
fn recording_host(
    socket: &std::path::Path,
) -> anyhow::Result<(tokio::task::JoinHandle<()>, Recorded)> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let listener = tokio::net::UnixListener::bind(socket)?;
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            tokio::io::BufReader::new(read)
                .read_line(&mut line)
                .await
                .unwrap();
            seen.lock()
                .unwrap()
                .push(serde_json::from_str::<Value>(&line).unwrap());
            let value = json!({"state":"completed","result":{"models":[]}});
            let reply = json!({"ok":true,"value":value,"error":null});
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
    config.work.model = "mock".into();
    config.work.storage_root = storage.to_str().unwrap().into();
    config.work.secondary_root = None;
    config.sandbox.enabled = true;
    config.sandbox.host_socket = socket.to_str().unwrap().into();
    config.sandbox.image = format!("review@sha256:{}", "c".repeat(64));
    config
}

#[tokio::test]
#[ignore = "requires local PostgreSQL and NATS"]
async fn runs_wait_for_idle_suspension_and_models_are_discovered_once() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let temp = tempfile::tempdir()?;
    let socket = temp.path().join("host.sock");
    let (stub, requests) = recording_host(&socket)?;
    let outcome = async {
        let user = h.user().await?;
        let org = id(&user, "org_id");
        let conv = h.conversation(&user).await?;
        sqlx::query("UPDATE conversations SET mode='work',run_config='{}' WHERE id=$1").bind(conv).execute(&h.db).await?;
        let ctx = h.store.context(id(&user, "user_id"), org).await?;
        let lease = h.store.claim(org, conv, Uuid::now_v7(), 60_000).await?.unwrap();
        let config = work_config(&socket, temp.path());
        h.store.accept(&ctx, conv, "review", &message(Uuid::now_v7(), "turn")).await?;
        owner::process_with_config(&h.store, &lease, &config).await?;
        let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1").bind(conv).fetch_one(&h.db).await?;
        sqlx::query("INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent,node_lease_until) VALUES($1,$2,$3,$4,1,$5,'running','{}',true,now()+interval '1 hour')")
            .bind(Uuid::now_v7()).bind(org).bind(conv).bind(run).bind(lease.epoch).execute(&h.db).await?;
        let suspension = Uuid::now_v7();
        sqlx::query("INSERT INTO workspace_jobs(id,org_id,conversation_id,kind,state) VALUES($1,$2,$3,'suspend','running')")
            .bind(suspension).bind(org).bind(conv).execute(&h.db).await?;
        let app = App::new(h.store.clone(), h.nats.clone(), config)?;
        // The suspension is about to stop the sandbox: the new run must not start using it.
        let waiting = tokio::time::timeout(Duration::from_millis(1500), ryzhub_server::work::ensure_work(&app, &lease, run)).await;
        ensure!(waiting.is_err(), "a run must wait for an in-progress suspension");
        ensure!(requests.lock().unwrap().is_empty(), "nothing may touch the sandbox while it is being suspended");
        sqlx::query("UPDATE workspace_jobs SET state='completed' WHERE id=$1").bind(suspension).execute(&h.db).await?;
        // Later runs reuse the recorded models: the guest forgets old results eventually.
        ryzhub_server::work::ensure_work(&app, &lease, run).await?;
        ryzhub_server::work::ensure_work(&app, &lease, run).await?;
        let discoveries = requests.lock().unwrap().iter()
            .filter(|r| r["request"]["command"]["op_id"].as_str().is_some_and(|op| op.ends_with(":models")))
            .count();
        ensure!(discoveries == 1, "model discovery ran {discoveries} times");
        Ok::<_, anyhow::Error>(())
    }.await;
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
    let (stub, requests) = recording_host(&socket)?;
    let outcome = async {
        let user = h.user().await?;
        let org = id(&user, "org_id");
        let conv = h.conversation(&user).await?;
        sqlx::query("UPDATE conversations SET mode='work',run_config='{}' WHERE id=$1").bind(conv).execute(&h.db).await?;
        let ctx = h.store.context(id(&user, "user_id"), org).await?;
        let lease = h.store.claim(org, conv, Uuid::now_v7(), 60_000).await?.unwrap();
        let config = work_config(&socket, temp.path());
        h.store.accept(&ctx, conv, "review", &message(Uuid::now_v7(), "turn")).await?;
        owner::process_with_config(&h.store, &lease, &config).await?;
        let run: Uuid = sqlx::query_scalar("SELECT id FROM runs WHERE conversation_id=$1").bind(conv).fetch_one(&h.db).await?;
        sqlx::query("UPDATE runs SET state='completed',ended_at=now() WHERE id=$1").bind(run).execute(&h.db).await?;
        let sandbox = Uuid::now_v7();
        sqlx::query("INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent,native_thread_id) VALUES($1,$2,$3,$4,1,$5,'suspended','{}',true,'thread')")
            .bind(sandbox).bind(org).bind(conv).bind(run).bind(lease.epoch).execute(&h.db).await?;
        let head = Uuid::now_v7();
        let manifest = json!({"image":config.sandbox.image,"format":1,"encoding":"tar.zst","sha256":"d".repeat(64),"thread_id":"thread"});
        sqlx::query("INSERT INTO checkpoints(id,org_id,conversation_id,sandbox_id,manifest,channel_seq,gseq) VALUES($1,$2,$3,$4,$5,0,0)")
            .bind(head).bind(org).bind(conv).bind(sandbox).bind(manifest).execute(&h.db).await?;
        sqlx::query("INSERT INTO workspace_heads(org_id,conversation_id,checkpoint_id) VALUES($1,$2,$3)").bind(org).bind(conv).bind(head).execute(&h.db).await?;
        let app = App::new(h.store.clone(), h.nats.clone(), config)?;
        let link = OwnerLink::detached();
        let job_state = |job: Uuid| {
            let db = h.db.clone();
            async move {
                sqlx::query_scalar::<_, String>("SELECT state FROM workspace_jobs WHERE id=$1").bind(job).fetch_one(&db).await
            }
        };
        // A manual checkpoint of a suspended workspace: its head is already current.
        let checkpoint = Uuid::now_v7();
        sqlx::query("INSERT INTO workspace_jobs(id,org_id,conversation_id,kind) VALUES($1,$2,$3,'checkpoint')").bind(checkpoint).bind(org).bind(conv).execute(&h.db).await?;
        ryzhub_server::workspace::jobs(&app, &lease, &link).await?;
        ensure!(job_state(checkpoint).await? == "completed", "checkpoint of a suspended workspace failed");
        // A fork starts from that head, with a pin of its own.
        let fork = Uuid::now_v7();
        sqlx::query("INSERT INTO conversation_operations(id,org_id,conversation_id,kind,payload) VALUES($1,$2,$3,'fork','{\"seq\":0}')").bind(fork).bind(org).bind(conv).execute(&h.db).await?;
        ryzhub_server::conversation_commands::jobs(&app, &lease, &link).await?;
        let forked: String = sqlx::query_scalar("SELECT state FROM conversation_operations WHERE id=$1").bind(fork).fetch_one(&h.db).await?;
        ensure!(forked == "completed", "fork of a suspended workspace failed");
        let pinned: Uuid = sqlx::query_scalar("SELECT checkpoint_id FROM restore_pins WHERE id=$1").bind(fork).fetch_one(&h.db).await?;
        ensure!(pinned == head, "the fork must pin the suspended head");
        // Archiving needs no new checkpoint either.
        let archive = Uuid::now_v7();
        sqlx::query("INSERT INTO workspace_jobs(id,org_id,conversation_id,kind) VALUES($1,$2,$3,'archive')").bind(archive).bind(org).bind(conv).execute(&h.db).await?;
        ryzhub_server::workspace::jobs(&app, &lease, &link).await?;
        ensure!(job_state(archive).await? == "completed", "archive of a suspended workspace failed");
        let states: (String, String) = sqlx::query_as("SELECT c.state,s.state FROM conversations c JOIN sandboxes s ON s.conversation_id=c.id WHERE c.id=$1").bind(conv).fetch_one(&h.db).await?;
        ensure!(states == ("archived".into(), "archived".into()), "unexpected states {states:?}");
        ensure!(requests.lock().unwrap().is_empty(), "a suspended workspace needs no execution node");
        Ok::<_, anyhow::Error>(())
    }.await;
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
        sqlx::query("UPDATE conversations SET mode='work' WHERE id=$1").bind(conv).execute(&h.db).await?;
        // Idle for ten minutes: past a 60-second organization limit, within the global 1800.
        let run = Uuid::now_v7();
        sqlx::query("INSERT INTO runs(id,org_id,conversation_id,exec_epoch,state,response_text,assistant_item_id,cause_cid,ended_at) VALUES($1,$2,$3,1,'cancelled','',$4,$5,now()-interval '10 minutes')")
            .bind(run).bind(org).bind(conv).bind(Uuid::now_v7()).bind(Uuid::now_v7()).execute(&h.db).await?;
        sqlx::query("INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent,node_lease_until,created_at) VALUES($1,$2,$3,$4,1,1,'running','{}',true,now()+interval '1 hour',now()-interval '10 minutes')")
            .bind(Uuid::now_v7()).bind(org).bind(conv).bind(run).execute(&h.db).await?;
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
        ensure!(!woken(global.clone()).await?, "within the global idle limit");
        sqlx::query("INSERT INTO org_policies(org_id,version,policy) VALUES($1,(SELECT coalesce(max(version),0)+1 FROM org_policies WHERE org_id=$1),'{\"compute_idle_ttl_secs\":60}')")
            .bind(org).execute(&h.db).await?;
        ensure!(woken(global).await?, "the organization's shorter limit must wake the workspace");
        ensure!(woken(disabled.clone()).await?, "an organization limit applies without a global one");
        sqlx::query("DELETE FROM org_policies WHERE org_id=$1").bind(org).execute(&h.db).await?;
        ensure!(!woken(disabled).await?, "no limit at all: never suspend");
        Ok::<_, anyhow::Error>(())
    }.await;
    worker.close().await;
    h.close().await?;
    outcome
}
