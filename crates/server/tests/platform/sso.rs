//! Signing in with other accounts against a local OpenID Connect provider and a GitHub stand-in.
use super::*;
use axum::{extract::Query, response::IntoResponse, routing::get};
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// A test-only key, nothing else signs with it.
pub(super) const KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCtoBqJV/aNbbHr
G15FNspFHyN/+r9B2Az9Zp/+zFShEgWQR5bA7D05AhcjLLaN3EGiOtBg8C0UEBxH
1L+5mzXZpPcBcT/aTeOToQd/YmES6ArdlyV/QDRRnRCEXky/yauWN7SCA7xrdes0
wL4OF2KawlsX9XNsuhEwMOTWmRBSPpXD013g9GDNfVQMxXtvgS+T+3rcuiq9vmyv
hwG8s0ZS315tCKT5iE/KB3VU9+4zxWCYji3nn6GO0ZcJOtzNLISPTEN8EMH6mTqi
Yp7fUHvwqQHqHwb3L8Kwq/i+wsuiPrcNDs1ebfHzWrWZlLydZW+T+Vin3mHTi1oN
Bg4DQ4FpAgMBAAECggEAKinGJ/I3inLIEQsNYouCpTSBn3OZ/Hhl9ydz1SFzr8W7
miT4ZBM6btymF8Gk92A0hBv6PIA2wjeDVqJAiOc7NYZNAYmp8u6VRGWHd5TBdCDr
epJY5cUGr4cP2sU3qB9mTs1qRWR43jO9kYr43F0rcd+e1XuisxWgVO6as85p2z62
Yktkd1vXG3FR4YZkMBDs6QfoFWS2qvHri39gA1H383buQRj2eDq0f651oroYx/Qa
HuVcaC2Wvuk6OyyWeui/ArBzydd/1uf+/deLyLCpZvwPLh4N2kmC7Tm7d8DdHsQr
rA8pgZLvyMvQJxgdTkycH/fk4Iwe2kWy3kB/g26nMQKBgQDuGQ0ix5u4RX0Ch1Lu
m2o2WxgavaI5pDmdpCSBeO7hR2RBf+UDm8LKcxcibgw2jtqNncHGycHNobYJ/thV
SSXP/2nfVpRAKbOhrMqU0wAJhRGd5lA5BT0dyEl9kkwCj4bJ9teruG2pFHRSHxw4
c9y7ZimAlt2xp4C8Jci+RqitTQKBgQC6rhNQ8nJfGLs1yt4uuKFK7+EW6WdOcvXl
0JPgCttsugrK9FNtW+MChscPPl2YHawzyC+UsbU+1pKERH0LMwhEU6tGVdnGkyW/
eHTdyxCBe2E+kKoU9PdONh12oTkVa66zvQpO6sqeKPrdvtRj0uYraT9Rlb1pkRC6
HjKWFF1GjQKBgQDpHfH0iYb3ZmG9Jg2WvOfDDHGPTPgWg6o9pEZx1ZvdSe7pCnWJ
kOmpGgIenayrhjaJzVm6bjOoR5paMwBQkGvLOIHKP5SrNalAtFMsX2J776ON+7+z
9GMgU8dQgPqWzzvjZiFHai1EDFjPRtJ0m0wU1zPvSMcKd7vdewkO81OXSQKBgH/j
9fPExToFFWxa2TAk6MsP36L0sAN8mp2g8U3R4lVwdyhnjigjhTgb4Y+96BzoZyrq
gD5cKJEuKx5WVVwwz/8PWw1Kj3GNKZUtDtNL5LjwL8TPEtZcpHh+dlzpQtQ7+2nD
nqZGEB2ecY8sxxDEUU94QRrBfQwvh8gaHAnZVbWZAoGAesqqQBVqfPuTjTfBRQ+4
HvDh8KEX0XPW+g+EHXPQJE5VNIVq+j+zVhrkDUMoyyiQGQdrZS4DdUjnUVEbgeYw
mBysCXQPZEHN1kKAoLKhqkB6EUMNrIrwP43YN8U5Thazo6AXHzX4kpe9GAJ8GXuY
WWbsTPpwe/eYbNomgCTjOtA=
-----END PRIVATE KEY-----";
pub(super) const MODULUS: &str = "raAaiVf2jW2x6xteRTbKRR8jf_q_QdgM_Waf_sxUoRIFkEeWwOw9OQIXIyy2jdxBojrQYPAtFBAcR9S_uZs12aT3AXE_2k3jk6EHf2JhEugK3Zclf0A0UZ0QhF5Mv8mrlje0ggO8a3XrNMC-DhdimsJbF_VzbLoRMDDk1pkQUj6Vw9Nd4PRgzX1UDMV7b4Evk_t63Loqvb5sr4cBvLNGUt9ebQik-YhPygd1VPfuM8VgmI4t55-hjtGXCTrczSyEj0xDfBDB-pk6omKe31B78KkB6h8G9y_CsKv4vsLLoj63DQ7NXm3x81q1mZS8nWVvk_lYp95h04taDQYOA0OBaQ";

#[derive(Default)]
struct Provider {
    base: String,
    /// What the next ID token says differently: a nonce, an audience, a key.
    tamper: Option<&'static str>,
    email: String,
    verified: bool,
    subject: String,
    /// The PKCE challenge and nonce of the authorization in progress.
    challenge: String,
    nonce: String,
}
type Shared = Arc<Mutex<Provider>>;

#[derive(serde::Deserialize)]
struct Authorize {
    redirect_uri: String,
    state: String,
    code_challenge: String,
    #[serde(default)]
    nonce: String,
}

fn verifier_matches(challenge: &str, form: &HashMap<String, String>) -> bool {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(
        form.get("code_verifier")
            .cloned()
            .unwrap_or_default()
            .as_bytes(),
    )) == challenge
}

/// The form of a token request.
fn form(body: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}

/// An ID token for the code whose PKCE verifier matches, tampered with as the test asks.
async fn oidc_token(State(s): State<Shared>, body: String) -> axum::response::Response {
    let form = form(&body);
    let provider = s.lock().unwrap();
    if !verifier_matches(&provider.challenge, &form)
        || form.get("code").map(String::as_str) != Some("code-1")
    {
        let refused = Json(json!({"error":"invalid_grant"}));
        return (axum::http::StatusCode::BAD_REQUEST, refused).into_response();
    }
    let now = chrono::Utc::now().timestamp();
    let tampered = |what: &str| provider.tamper == Some(what);
    let claims = json!({
        "iss": provider.base, "sub": provider.subject,
        "aud": if tampered("audience") { "someone-else" } else { "ryzhub" },
        "exp": now + 300, "iat": now,
        "nonce": if tampered("nonce") { "replayed" } else { provider.nonce.as_str() },
        "email": provider.email, "email_verified": provider.verified, "name": "SSO User",
    });
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("k1".into());
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(KEY.as_bytes()).unwrap();
    let mut token = jsonwebtoken::encode(&header, &claims, &key).unwrap();
    if tampered("signature") {
        // The last character holds the signature's last bits: another one changes it.
        let last = token.pop().unwrap();
        token.push(if last == 'A' { 'Q' } else { 'A' });
    }
    Json(json!({"access_token":"oidc-access","token_type":"Bearer","id_token":token}))
        .into_response()
}

async fn serve_provider() -> anyhow::Result<(Shared, String, JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let shared: Shared = Arc::new(Mutex::new(Provider {
        base: base.clone(),
        email: "sso-user@example.test".into(),
        verified: true,
        subject: "corp-user-1".into(),
        ..Default::default()
    }));
    let authorize = |State(s): State<Shared>, Query(q): Query<Authorize>| async move {
        let mut provider = s.lock().unwrap();
        provider.challenge = q.code_challenge;
        provider.nonce = q.nonce;
        let mut target = url::Url::parse(&q.redirect_uri).unwrap();
        target
            .query_pairs_mut()
            .append_pair("code", "code-1")
            .append_pair("state", &q.state);
        axum::response::Redirect::to(target.as_str())
    };
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(|State(s): State<Shared>| async move {
                let base = s.lock().unwrap().base.clone();
                Json(
                    json!({"issuer": base, "authorization_endpoint": format!("{base}/authorize"),
                    "token_endpoint": format!("{base}/token"), "jwks_uri": format!("{base}/jwks")}),
                )
            }),
        )
        .route(
            "/jwks",
            get(|| async {
                let key = json!({"kty":"RSA","kid":"k1","alg":"RS256","use":"sig",
                    "n":MODULUS,"e":"AQAB"});
                Json(json!({"keys":[key]}))
            }),
        )
        .route("/authorize", get(authorize))
        .route("/token", post(oidc_token))
        .route("/gh/login/oauth/authorize", get(authorize))
        .route(
            "/gh/login/oauth/access_token",
            post(|State(s): State<Shared>, body: String| async move {
                let ok = verifier_matches(&s.lock().unwrap().challenge, &form(&body));
                Json(if ok {
                    json!({"access_token":"gh-token"})
                } else {
                    json!({"error":"bad_verification_code"})
                })
            }),
        )
        .route(
            "/gh/api/user",
            get(|headers: HeaderMap| async move {
                let authorization = headers.get("authorization").and_then(|h| h.to_str().ok());
                if authorization != Some("Bearer gh-token") {
                    return axum::http::StatusCode::UNAUTHORIZED.into_response();
                }
                Json(json!({"id": 4242, "login": "octo", "name": "Octo"})).into_response()
            }),
        )
        .route(
            "/gh/api/user/emails",
            get(|State(s): State<Shared>| async move {
                let provider = s.lock().unwrap();
                Json(
                    json!([{"email":"other@example.test","primary":false,"verified":true},
                            {"email":provider.email,"primary":true,"verified":provider.verified}]),
                )
            }),
        )
        .with_state(shared.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Ok((shared, base, task))
}

/// Sign in through `provider` as a browser would; the address the client is sent back to.
async fn through(base: &str, provider: &str, link: Option<&str>) -> anyhow::Result<url::Url> {
    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut start = url::Url::parse(&format!("{base}/auth/oauth/{provider}/start"))?;
    start
        .query_pairs_mut()
        .append_pair("return_to", "http://client.test/app");
    if let Some(ticket) = link {
        start.query_pairs_mut().append_pair("link", ticket);
    }
    let mut next = start.to_string();
    // To the provider, back to the platform, on to the client.
    for _ in 0..3 {
        let response = browser.get(&next).send().await?;
        ensure!(
            response.status().is_redirection(),
            "{next}: {}",
            response.status()
        );
        next = response.headers()["location"].to_str()?.to_owned();
        if next.starts_with("http://client.test") {
            return Ok(url::Url::parse(&next)?);
        }
    }
    anyhow::bail!("never returned to the client")
}

fn param(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

async fn exchange(h: &Harness, code: &str) -> anyhow::Result<(u16, Value)> {
    let response = reqwest::Client::new()
        .post(format!("{}/auth/oauth/exchange", h.base))
        .json(&json!({"code": code}))
        .send()
        .await?;
    Ok((
        response.status().as_u16(),
        response.json().await.unwrap_or(Value::Null),
    ))
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and test gateway secrets"]
async fn members_sign_in_with_oidc_and_github_and_keep_one_way_in() -> anyhow::Result<()> {
    let (provider, base, task) = serve_provider().await?;
    let issuer = base.clone();
    let h = Harness::with(move |c| {
        c.auth.oidc = vec![ryzhub_server::config::OidcProvider {
            name: "corp".into(),
            label: "Corp SSO".into(),
            issuer: issuer.clone(),
            client_id: "ryzhub".into(),
            ..Default::default()
        }];
        c.auth.github = Some(ryzhub_server::config::GithubProvider {
            client_id: "gh-client".into(),
            client_secret_env: "RYZHUB_TEST_UPSTREAM_KEY".into(),
            web_url: format!("{issuer}/gh"),
            api_url: format!("{issuer}/gh/api"),
            ..Default::default()
        });
        c.auth.return_urls = vec!["http://client.test/".into(), "http://127.0.0.1:".into()];
    })
    .await?;
    let caps: Value = reqwest::get(format!("{}/capabilities", h.base))
        .await?
        .json()
        .await?;
    assert_eq!(caps["auth_providers"].as_array().map(Vec::len), Some(2));

    // A first sign-in makes the account; the code works once.
    let back = through(&h.base, "corp", None).await?;
    let code = param(&back, "login_code").expect("a login code");
    let (status, session) = exchange(&h, &code).await?;
    assert_eq!(status, 200, "{session}");
    assert!(session["token"].is_string() && session["org_id"].is_string());
    let user = session["user_id"].clone();
    assert_eq!(exchange(&h, &code).await?.0, 401, "spent");
    // The same identity signs in to the same account.
    let again = through(&h.base, "corp", None).await?;
    assert_eq!(
        exchange(&h, &param(&again, "login_code").unwrap()).await?.1["user_id"],
        user
    );

    // What the provider signs is checked: nonce, audience, signature.
    for tamper in ["nonce", "audience", "signature"] {
        provider.lock().unwrap().tamper = Some(tamper);
        let refused = through(&h.base, "corp", None).await?;
        assert_eq!(
            param(&refused, "login_error").as_deref(),
            Some("LOGIN_FAILED"),
            "{tamper}"
        );
        assert!(param(&refused, "login_code").is_none());
    }
    provider.lock().unwrap().tamper = None;

    // GitHub, with the verified primary email of an existing account: joined to it.
    let member = h.user().await?;
    let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id=$1::uuid")
        .bind(member["user_id"].as_str().unwrap())
        .fetch_one(&h.db)
        .await?;
    provider.lock().unwrap().email = email.clone();
    let joined = through(&h.base, "github", None).await?;
    let (_, session) = exchange(&h, &param(&joined, "login_code").unwrap()).await?;
    assert_eq!(session["user_id"], member["user_id"]);
    // An unverified email does not take over the account that has it.
    provider.lock().unwrap().verified = false;
    provider.lock().unwrap().subject = "corp-user-2".into();
    let refused = through(&h.base, "corp", None).await?;
    assert_eq!(
        param(&refused, "login_error").as_deref(),
        Some("ACCOUNT_EXISTS")
    );
    provider.lock().unwrap().verified = true;

    // A signed-in member adds an identity; one with a password may remove them all.
    provider.lock().unwrap().subject = "corp-user-3".into();
    let ticket: Value = reqwest::Client::new()
        .post(format!("{}/auth/ticket", h.base))
        .bearer_auth(member["token"].as_str().unwrap())
        .send()
        .await?
        .json()
        .await?;
    let linked = through(&h.base, "corp", ticket["ticket"].as_str()).await?;
    assert_eq!(param(&linked, "login_linked").as_deref(), Some("corp"));
    let listed: Value = reqwest::Client::new()
        .get(format!("{}/me/identities", h.base))
        .bearer_auth(member["token"].as_str().unwrap())
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(listed["password"], true);
    assert_eq!(listed["identities"].as_array().map(Vec::len), Some(2));
    for identity in listed["identities"].as_array().unwrap() {
        let removed = reqwest::Client::new()
            .delete(format!(
                "{}/me/identities/{}/{}",
                h.base,
                identity["provider"].as_str().unwrap(),
                identity["subject"].as_str().unwrap()
            ))
            .bearer_auth(member["token"].as_str().unwrap())
            .send()
            .await?;
        assert_eq!(removed.status(), 200);
    }
    // The account made by signing in has no password: its only identity stays.
    provider.lock().unwrap().subject = "corp-user-1".into();
    let fresh = through(&h.base, "corp", None).await?;
    let (_, session) = exchange(&h, &param(&fresh, "login_code").unwrap()).await?;
    let last = reqwest::Client::new()
        .delete(format!("{}/me/identities/corp/corp-user-1", h.base))
        .bearer_auth(session["token"].as_str().unwrap())
        .send()
        .await?;
    assert_eq!(last.status(), 409);
    // Its password is not one: password sign-in fails.
    let login = reqwest::Client::new()
        .post(format!("{}/auth/login", h.base))
        .json(&json!({"email":"sso-user@example.test","password":"!"}))
        .send()
        .await?;
    assert_eq!(login.status(), 401);

    // A return address the instance does not list is refused, and so is one that only starts
    // like a listed one: credentials before `@` would send the browser to another host.
    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    for return_to in [
        "https://evil.example/",
        "http://127.0.0.1:x@evil.example/",
        "http://127.0.0.1:@evil.example/",
    ] {
        let mut start = url::Url::parse(&format!("{}/auth/oauth/corp/start", h.base))?;
        start.query_pairs_mut().append_pair("return_to", return_to);
        let refused = browser.get(start).send().await?;
        assert_eq!(refused.status(), 400, "{return_to}");
    }
    task.abort();
    h.close().await
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS and test gateway secrets"]
async fn closing_registration_lets_members_in_and_keeps_newcomers_out() -> anyhow::Result<()> {
    let (provider, issuer, task) = serve_provider().await?;
    let mut h = Harness::with(move |c| {
        c.auth.oidc = vec![ryzhub_server::config::OidcProvider {
            name: "corp".into(),
            label: "Corp SSO".into(),
            issuer,
            client_id: "ryzhub".into(),
            ..Default::default()
        }];
        c.auth.return_urls = vec!["http://client.test/".into()];
    })
    .await?;
    // Members from before sign-up closed: one with a password, one through the provider.
    let member = h.user().await?;
    let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id=$1::uuid")
        .bind(member["user_id"].as_str().unwrap())
        .fetch_one(&h.db)
        .await?;
    through(&h.base, "corp", None).await?;

    // The same instance with sign-up closed.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let mut config = (*h.app.config).clone();
    config.public_url = base.clone();
    config.auth.registration = false;
    let closed = App::new(h.app.store.clone(), h.app.nats.clone(), config)?;
    h.serve(listener, ryzhub_server::http::router(closed));
    let caps: Value = reqwest::get(format!("{base}/capabilities"))
        .await?
        .json()
        .await?;
    assert_eq!(caps["registration"], false);

    let client = reqwest::Client::new();
    let register = client
        .post(format!("{base}/auth/register"))
        .json(&json!({"email":"newcomer@example.test","password":"test-password"}))
        .send()
        .await?;
    assert_eq!(register.status(), 403);
    assert_eq!(
        register.json::<Value>().await?["error"],
        "REGISTRATION_CLOSED"
    );
    let login = client
        .post(format!("{base}/auth/login"))
        .json(&json!({"email":email,"password":"test-password"}))
        .send()
        .await?;
    assert_eq!(login.status(), 200);

    // A known identity signs in, and a verified email joins its account; nobody new gets in.
    let again = through(&base, "corp", None).await?;
    assert!(param(&again, "login_code").is_some(), "{again}");
    provider.lock().unwrap().subject = "corp-user-2".into();
    provider.lock().unwrap().email = email;
    let joined = through(&base, "corp", None).await?;
    let (_, session) = exchange(&h, &param(&joined, "login_code").unwrap()).await?;
    assert_eq!(session["user_id"], member["user_id"]);
    provider.lock().unwrap().subject = "corp-user-3".into();
    provider.lock().unwrap().email = "newcomer@example.test".into();
    let refused = through(&base, "corp", None).await?;
    assert_eq!(
        param(&refused, "login_error").as_deref(),
        Some("REGISTRATION_CLOSED")
    );
    let created: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email=$1")
        .bind("newcomer@example.test")
        .fetch_one(&h.db)
        .await?;
    assert_eq!(created, 0);
    task.abort();
    h.close().await
}
