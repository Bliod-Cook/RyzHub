//! Android notifications through an FCM stand-in: approvals, questions and finished runs reach
//! signed-in devices without a live connection, once.
use super::*;
use axum::{extract::Form, routing::get};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio_tungstenite::tungstenite::{Message as WsMessage, client::IntoClientRequest};

type Sent = Arc<Mutex<Vec<Value>>>;

async fn token(Form(form): Form<HashMap<String, String>>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let key = jsonwebtoken::DecodingKey::from_rsa_components(super::sso::MODULUS, "AQAB").unwrap();
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.validate_aud = false;
    let claims = jsonwebtoken::decode::<Value>(
        form.get("assertion")
            .map(String::as_str)
            .unwrap_or_default(),
        &key,
        &validation,
    );
    match claims {
        Ok(data)
            if form.get("grant_type").map(String::as_str)
                == Some("urn:ietf:params:oauth:grant-type:jwt-bearer")
                && data.claims["iss"] == "push@ryzhub-test.iam.gserviceaccount.com"
                && data.claims["scope"] == "https://www.googleapis.com/auth/firebase.messaging" =>
        {
            Json(json!({"access_token": "fcm-access", "expires_in": 3600})).into_response()
        }
        _ => (axum::http::StatusCode::UNAUTHORIZED, "bad assertion").into_response(),
    }
}

async fn send(
    State(sent): State<Sent>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if headers["authorization"] != "Bearer fcm-access" {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    sent.lock().unwrap().push(body.clone());
    if body["message"]["token"] == "tok-dead" {
        let error = json!({"error": {"code": 404, "status": "NOT_FOUND", "details": [
            {"@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError", "errorCode": "UNREGISTERED"}]}});
        return (axum::http::StatusCode::NOT_FOUND, Json(error)).into_response();
    }
    Json(json!({"name": "projects/ryzhub-test/messages/1"})).into_response()
}

async fn register(h: &Harness, user: &Value, device: &str, token: &str) -> anyhow::Result<()> {
    reqwest::Client::new()
        .post(format!("{}/devices", h.base))
        .bearer_auth(user["token"].as_str().unwrap())
        .json(&json!({"device": device, "platform": "android", "token": token, "locale": "en"}))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn delivered(sent: &Sent, token: &str, count: usize) -> anyhow::Result<Vec<Value>> {
    for _ in 0..200 {
        let to: Vec<Value> = sent
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m["message"]["token"] == token)
            .cloned()
            .collect();
        if to.len() >= count {
            return Ok(to);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("no notification to {token}")
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS"]
async fn finished_runs_notify_offline_devices_of_signed_in_users() -> anyhow::Result<()> {
    let sent: Sent = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let fcm = format!("http://{}", listener.local_addr()?);
    let fixture = tokio::spawn({
        let router = Router::new()
            .route("/token", post(token))
            .route("/v1/projects/ryzhub-test/messages:send", post(send))
            .route("/", get(|| async { "fcm" }))
            .with_state(sent.clone());
        async move { axum::serve(listener, router).await.unwrap() }
    });
    let mut account = tempfile::NamedTempFile::new()?;
    write!(
        account,
        "{}",
        json!({"type": "service_account", "project_id": "ryzhub-test",
               "client_email": "push@ryzhub-test.iam.gserviceaccount.com",
               "private_key": super::sso::KEY, "token_uri": format!("{fcm}/token")})
    )?;
    let path = account.path().display().to_string();
    let h = Harness::with(|config| {
        config.push.enabled = true;
        config.push.fcm_credentials = path;
        config.push.fcm_endpoint = fcm.clone();
    })
    .await?;
    let capabilities: Value = reqwest::get(format!("{}/capabilities", h.base))
        .await?
        .json()
        .await?;
    assert_eq!(capabilities["push"], true);
    let user = h.user().await?;
    let conv = h.conversation(&user).await?;
    let org = id(&user, "org_id");
    register(&h, &user, "phone", "tok-phone").await?;
    register(&h, &user, "old-phone", "tok-dead").await?;
    let mut owner = h.owner()?;

    // A finished run reaches both devices; the token FCM no longer knows is forgotten.
    h.send(&user, conv, message("hello")).await?;
    let first = h.wait_state(conv, "completed").await?;
    let to_phone = delivered(&sent, "tok-phone", 1).await?;
    let note = &to_phone[0]["message"];
    assert_eq!(note["data"]["conversation_id"], conv.to_string());
    assert_eq!(note["data"]["org_id"], org.to_string());
    assert_eq!(note["notification"]["title"], "Platform");
    assert_eq!(note["notification"]["body"], "Done.");
    assert_eq!(note["android"]["priority"], "high");
    delivered(&sent, "tok-dead", 1).await?;
    let mut forgotten = false;
    for _ in 0..100 {
        forgotten = !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM push_devices WHERE token='tok-dead')",
        )
        .fetch_one(&h.db)
        .await?;
        if forgotten {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(forgotten);

    // A device with a live connection already shows the change.
    let mut request = format!("{}/ws", h.base.replace("http:", "ws:")).into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", user["token"].as_str().unwrap()).parse()?,
    );
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", SUBPROTOCOL.parse()?);
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await?;
    let hello = Frame {
        body: Some(frame::Body::Hello(Hello {
            protocol_major: 1,
            device_id: "phone".into(),
            ..Default::default()
        })),
        ..Default::default()
    };
    socket
        .send(WsMessage::Binary(hello.encode_to_vec().into()))
        .await?;
    socket.next().await.context("welcome")??;
    let online: bool =
        sqlx::query_scalar("SELECT online_until>now() FROM push_devices WHERE token='tok-phone'")
            .fetch_one(&h.db)
            .await?;
    assert!(online);
    h.send(&user, conv, message("hello again")).await?;
    let second = h.wait_after(conv, first).await?;
    let mut finished = false;
    for _ in 0..200 {
        finished = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM push_jobs WHERE payload->>'run'=$1 AND state='completed')",
        )
        .bind(second.to_string())
        .fetch_one(&h.db)
        .await?;
        if finished {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(finished);
    assert_eq!(
        delivered(&sent, "tok-phone", 1).await?.len(),
        1,
        "not while connected"
    );
    socket.close(None).await?;

    // Signing out stops notifications to the devices it registered.
    let mut offline = false;
    for _ in 0..100 {
        offline = sqlx::query_scalar(
            "SELECT online_until IS NULL FROM push_devices WHERE token='tok-phone'",
        )
        .fetch_one(&h.db)
        .await?;
        if offline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(offline, "the closed connection is no longer online");
    reqwest::Client::new()
        .post(format!("{}/auth/logout", h.base))
        .bearer_auth(user["token"].as_str().unwrap())
        .send()
        .await?
        .error_for_status()?;
    h.send(&user, conv, message("after sign out")).await?;
    let third = h.wait_after(conv, second).await?;
    for _ in 0..200 {
        let done: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM push_jobs WHERE payload->>'run'=$1 AND state='completed')",
        )
        .bind(third.to_string())
        .fetch_one(&h.db)
        .await?;
        if done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        delivered(&sent, "tok-phone", 1).await?.len(),
        1,
        "signed out"
    );

    owner.kill().await?;
    owner.wait().await?;
    fixture.abort();
    h.close().await
}
