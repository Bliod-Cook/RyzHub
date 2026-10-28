//! Signing in with other accounts: OpenID Connect providers and GitHub.
//!
//! `/auth/oauth/<provider>/start` sends the browser to the provider with PKCE, a state and a
//! nonce. Its callback checks what comes back (an OIDC ID token against the provider's keys,
//! issuer, audience and nonce; GitHub's verified primary email) and finds the account: the
//! identity's; else, when the provider verified the email and the instance allows it, the
//! account with that email; else a new account with its personal organization. The browser
//! returns to the client with a one-use code, which `/auth/oauth/exchange` trades for a
//! session as a login would: tokens never travel in an address.
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect},
};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{auth, authenticate, user_orgs};
use crate::{
    app::App,
    config::{Config, GithubProvider, OidcProvider},
    error::{ApiResult, Code},
    store::{hash_token, new_token},
};

/// How long a member has to finish at the provider, and to exchange the code after.
const FLOW_MINUTES: i64 = 10;
const CODE_SECS: i64 = 120;

enum Provider<'a> {
    Oidc(&'a OidcProvider),
    Github(&'a GithubProvider),
}

fn provider<'a>(config: &'a Config, name: &str) -> anyhow::Result<Provider<'a>> {
    let auth = &config.auth;
    if name == "github" {
        return auth
            .github
            .as_ref()
            .map(Provider::Github)
            .context(Code::ProviderUnknown);
    }
    auth.oidc
        .iter()
        .find(|p| p.name == name)
        .map(Provider::Oidc)
        .context(Code::ProviderUnknown)
}

/// The providers clients offer: `[{id, label, kind}]`.
pub fn providers(app: &App) -> Vec<Value> {
    let auth = &app.config().auth;
    auth.oidc
        .iter()
        .map(|p| {
            json!({"id": p.name, "label": if p.label.is_empty() { &p.name } else { &p.label }, "kind": "oidc"})
        })
        .chain(
            auth.github
                .iter()
                .map(|g| json!({"id": "github", "label": g.label, "kind": "github"})),
        )
        .collect()
}

fn callback_url(app: &App, name: &str) -> String {
    format!(
        "{}/auth/oauth/{name}/callback",
        app.config().public_url.trim_end_matches('/')
    )
}

fn random_text(bytes: usize) -> String {
    let random: Vec<u8> = (0..bytes).map(|_| rand::random::<u8>()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
}

fn secret(env: &str) -> Option<String> {
    (!env.is_empty())
        .then(|| std::env::var(env).ok())
        .flatten()
        .filter(|s| !s.is_empty())
}

async fn get_json(app: &App, url: &str, bearer: Option<&str>) -> anyhow::Result<Value> {
    let mut request = app
        .http
        .identity
        .get(url)
        .header("accept", "application/json");
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.context(Code::LoginFailed)?;
    ensure!(response.status().is_success(), Code::LoginFailed);
    response.json().await.context(Code::LoginFailed)
}

/// An OIDC provider's metadata, which must be its own.
async fn discovery(app: &App, provider: &OidcProvider) -> anyhow::Result<Value> {
    let issuer = provider.issuer.trim_end_matches('/');
    let metadata = get_json(
        app,
        &format!("{issuer}/.well-known/openid-configuration"),
        None,
    )
    .await?;
    ensure!(
        metadata["issuer"].as_str().map(|i| i.trim_end_matches('/')) == Some(issuer),
        Code::LoginFailed
    );
    Ok(metadata)
}

#[derive(Deserialize)]
pub struct Start {
    return_to: String,
    /// A WebSocket ticket of the signed-in user, to add an identity to their account.
    #[serde(default)]
    link: Option<String>,
}

/// `GET /auth/oauth/{provider}/start`: off to the provider.
pub async fn start(
    State(app): State<App>,
    Path(name): Path<String>,
    Query(start): Query<Start>,
) -> ApiResult<Redirect> {
    let config = app.config();
    let provider = provider(&config, &name)?;
    // Credentials before `@` would make a listed prefix name another host.
    require!(
        url::Url::parse(&start.return_to)
            .is_ok_and(|url| url.username().is_empty() && url.password().is_none())
            && config
                .auth
                .return_urls
                .iter()
                .any(|prefix| start.return_to.starts_with(prefix.as_str())),
        Code::InvalidInput
    );
    let link_user: Option<Uuid> = match &start.link {
        Some(ticket) => Some(
            sqlx::query_scalar(
                "DELETE FROM ws_tickets t USING auth_sessions s
                 WHERE t.token_hash=$1 AND t.expires_at>now() AND s.id=t.session_id AND s.revoked_at IS NULL
                 RETURNING s.user_id",
            )
            .bind(hash_token(ticket))
            .fetch_optional(&app.store.pool)
            .await?
            .ok_or(Code::Unauthorized)?,
        ),
        None => None,
    };
    let state = random_text(24);
    let verifier = random_text(48);
    let nonce = random_text(24);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let redirect = callback_url(&app, &name);
    let mut url = match provider {
        Provider::Oidc(oidc) => {
            let metadata = discovery(&app, oidc).await?;
            let mut url = url::Url::parse(
                metadata["authorization_endpoint"]
                    .as_str()
                    .context(Code::LoginFailed)?,
            )
            .context(Code::LoginFailed)?;
            let scopes = if oidc.scopes.is_empty() {
                "openid email profile".to_owned()
            } else {
                oidc.scopes.join(" ")
            };
            url.query_pairs_mut()
                .append_pair("response_type", "code")
                .append_pair("client_id", &oidc.client_id)
                .append_pair("scope", &scopes)
                .append_pair("nonce", &nonce);
            url
        }
        Provider::Github(github) => {
            let mut url = url::Url::parse(&format!(
                "{}/login/oauth/authorize",
                github.web_url.trim_end_matches('/')
            ))
            .context(Code::LoginFailed)?;
            url.query_pairs_mut()
                .append_pair("client_id", &github.client_id)
                .append_pair("scope", "read:user user:email");
            url
        }
    };
    url.query_pairs_mut()
        .append_pair("redirect_uri", &redirect)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    sqlx::query(
        "INSERT INTO login_flows(state,provider,verifier,nonce,return_to,link_user,expires_at)
         VALUES($1,$2,$3,$4,$5,$6,now()+($7::bigint*interval '1 minute'))",
    )
    .bind(&state)
    .bind(&name)
    .bind(&verifier)
    .bind(&nonce)
    .bind(&start.return_to)
    .bind(link_user)
    .bind(FLOW_MINUTES)
    .execute(&app.store.pool)
    .await?;
    sqlx::query("DELETE FROM login_flows WHERE expires_at<now()")
        .execute(&app.store.pool)
        .await?;
    Ok(Redirect::to(url.as_str()))
}

#[derive(Deserialize)]
pub struct Returned {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Who the provider says signed in.
#[derive(Debug, PartialEq)]
pub struct Identity {
    pub subject: String,
    pub email: Option<String>,
    /// The provider checked the email belongs to them.
    pub verified: bool,
    pub name: Option<String>,
}

/// `return_to` with `name=value` added to its query.
fn with_param(return_to: &str, name: &str, value: &str) -> String {
    let mut url = url::Url::parse(return_to).expect("checked at start");
    url.query_pairs_mut().append_pair(name, value);
    url.into()
}

/// `GET /auth/oauth/{provider}/callback`: back from the provider, on to the client.
pub async fn callback(
    State(app): State<App>,
    Path(name): Path<String>,
    Query(returned): Query<Returned>,
) -> ApiResult<Redirect> {
    let state = returned.state.ok_or(Code::LoginFailed)?;
    // The state is spent whatever happens next.
    let flow: Option<(String, String, String, String, Option<Uuid>)> = sqlx::query_as(
        "DELETE FROM login_flows WHERE state=$1 AND expires_at>now()
         RETURNING provider,verifier,nonce,return_to,link_user",
    )
    .bind(&state)
    .fetch_optional(&app.store.pool)
    .await?;
    let (flow_provider, verifier, nonce, return_to, link_user) = flow.ok_or(Code::LoginFailed)?;
    require!(flow_provider == name, Code::LoginFailed);
    let outcome = async {
        ensure!(returned.error.is_none(), Code::LoginFailed);
        let code = returned.code.context(Code::LoginFailed)?;
        let config = app.config();
        let identity = match provider(&config, &name)? {
            Provider::Oidc(oidc) => {
                oidc_identity(&app, oidc, &name, &code, &verifier, &nonce).await?
            }
            Provider::Github(github) => github_identity(&app, github, &code, &verifier).await?,
        };
        account(&app, &name, &identity, link_user).await
    }
    .await;
    let target = match outcome {
        Ok(_) if link_user.is_some() => with_param(&return_to, "login_linked", &name),
        Ok(user) => {
            let code = new_token();
            // A disable either finds the code and deletes it, or comes first and refuses it.
            let mut tx = app.store.pool.begin().await?;
            crate::accounts::hold_active(&mut tx, user).await?;
            sqlx::query(
                "INSERT INTO login_codes(code_hash,user_id,expires_at) VALUES($1,$2,now()+($3::bigint*interval '1 second'))",
            )
            .bind(hash_token(&code))
            .bind(user)
            .bind(CODE_SECS)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            with_param(&return_to, "login_code", &code)
        }
        Err(error) => {
            tracing::info!(
                provider = name,
                error = format!("{error:#}"),
                "sign-in refused"
            );
            with_param(
                &return_to,
                "login_error",
                crate::error::public_code(&error).as_str(),
            )
        }
    };
    Ok(Redirect::to(&target))
}

/// The identity in an OIDC provider's ID token, checked against its keys, issuer, audience,
/// expiry and the nonce of this sign-in.
async fn oidc_identity(
    app: &App,
    provider: &OidcProvider,
    name: &str,
    code: &str,
    verifier: &str,
    nonce: &str,
) -> anyhow::Result<Identity> {
    let metadata = discovery(app, provider).await?;
    let mut form = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code.to_owned()),
        ("redirect_uri", callback_url(app, name)),
        ("client_id", provider.client_id.clone()),
        ("code_verifier", verifier.to_owned()),
    ];
    if let Some(secret) = secret(&provider.client_secret_env) {
        form.push(("client_secret", secret));
    }
    let tokens: Value = app
        .http
        .identity
        .post(
            metadata["token_endpoint"]
                .as_str()
                .context(Code::LoginFailed)?,
        )
        .header("accept", "application/json")
        .form(&form)
        .send()
        .await
        .context(Code::LoginFailed)?
        .error_for_status()
        .context(Code::LoginFailed)?
        .json()
        .await
        .context(Code::LoginFailed)?;
    let token = tokens["id_token"].as_str().context(Code::LoginFailed)?;
    let keys: jsonwebtoken::jwk::JwkSet = serde_json::from_value(
        get_json(
            app,
            metadata["jwks_uri"].as_str().context(Code::LoginFailed)?,
            None,
        )
        .await?,
    )
    .context(Code::LoginFailed)?;
    let claims = verify_id_token(token, &keys, &provider.issuer, &provider.client_id)?;
    ensure!(claims["nonce"] == nonce, Code::LoginFailed);
    Ok(Identity {
        subject: claims["sub"]
            .as_str()
            .context(Code::LoginFailed)?
            .to_owned(),
        email: claims["email"].as_str().map(|e| e.trim().to_lowercase()),
        verified: claims["email_verified"] == true,
        name: claims["name"].as_str().map(str::to_owned),
    })
}

/// An ID token's claims, once its signature (by one of `keys`), issuer, audience and expiry
/// check out.
pub fn verify_id_token(
    token: &str,
    keys: &jsonwebtoken::jwk::JwkSet,
    issuer: &str,
    audience: &str,
) -> anyhow::Result<Value> {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
    let header = decode_header(token).context(Code::LoginFailed)?;
    ensure!(
        matches!(
            header.alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::ES256
                | Algorithm::ES384
                | Algorithm::EdDSA
        ),
        Code::LoginFailed
    );
    let key = match &header.kid {
        Some(kid) => keys.find(kid),
        None if keys.keys.len() == 1 => keys.keys.first(),
        None => None,
    }
    .context(Code::LoginFailed)?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[issuer.trim_end_matches('/'), issuer]);
    validation.set_audience(&[audience]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    let decoded = decode::<Value>(
        token,
        &DecodingKey::from_jwk(key).context(Code::LoginFailed)?,
        &validation,
    )
    .context(Code::LoginFailed)?;
    Ok(decoded.claims)
}

/// The GitHub account: its id, and its primary email when GitHub verified it.
async fn github_identity(
    app: &App,
    github: &GithubProvider,
    code: &str,
    verifier: &str,
) -> anyhow::Result<Identity> {
    let form = [
        ("client_id", github.client_id.clone()),
        (
            "client_secret",
            secret(&github.client_secret_env).unwrap_or_default(),
        ),
        ("code", code.to_owned()),
        ("code_verifier", verifier.to_owned()),
    ];
    let tokens: Value = app
        .http
        .identity
        .post(format!(
            "{}/login/oauth/access_token",
            github.web_url.trim_end_matches('/')
        ))
        .header("accept", "application/json")
        .form(&form)
        .send()
        .await
        .context(Code::LoginFailed)?
        .error_for_status()
        .context(Code::LoginFailed)?
        .json()
        .await
        .context(Code::LoginFailed)?;
    let token = tokens["access_token"].as_str().context(Code::LoginFailed)?;
    let api = github.api_url.trim_end_matches('/');
    let user = get_json(app, &format!("{api}/user"), Some(token)).await?;
    let emails = get_json(app, &format!("{api}/user/emails"), Some(token)).await?;
    let primary = emails
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["primary"] == true && e["verified"] == true)
        .and_then(|e| e["email"].as_str())
        .map(|e| e.trim().to_lowercase());
    Ok(Identity {
        subject: user["id"]
            .as_i64()
            .map(|id| id.to_string())
            .context(Code::LoginFailed)?,
        verified: primary.is_some(),
        email: primary,
        name: user["name"]
            .as_str()
            .or_else(|| user["login"].as_str())
            .map(str::to_owned),
    })
}

/// The account `identity` signs in to; added to `link` when a signed-in user asked.
async fn account(
    app: &App,
    provider: &str,
    identity: &Identity,
    link: Option<Uuid>,
) -> anyhow::Result<Uuid> {
    let pool = &app.store.pool;
    let owner: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT i.user_id,u.status FROM user_identities i JOIN users u ON u.id=i.user_id
         WHERE i.provider=$1 AND i.subject=$2",
    )
    .bind(provider)
    .bind(&identity.subject)
    .fetch_optional(pool)
    .await?;
    if let Some((user, status)) = owner {
        ensure!(link.is_none_or(|l| l == user), Code::IdentityTaken);
        ensure!(status == "active", Code::AccountDisabled);
        return Ok(user);
    }
    let link_identity = |user: Uuid| async move {
        let mut tx = pool.begin().await?;
        crate::accounts::hold_active(&mut tx, user).await?;
        sqlx::query(
            "INSERT INTO user_identities(user_id,provider,subject,email) VALUES($1,$2,$3,$4)",
        )
        .bind(user)
        .bind(provider)
        .bind(&identity.subject)
        .bind(&identity.email)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        anyhow::Ok(user)
    };
    if let Some(user) = link {
        return link_identity(user).await;
    }
    let email = identity
        .email
        .clone()
        .context(Code::IdentityEmailRequired)?;
    let existing: Option<(Uuid, String)> =
        sqlx::query_as("SELECT id,status FROM users WHERE email=$1")
            .bind(&email)
            .fetch_optional(pool)
            .await?;
    if let Some((user, status)) = existing {
        // Joining an account by email needs the provider to vouch for the email.
        ensure!(
            identity.verified && app.config().auth.link_verified_email,
            Code::AccountExists
        );
        ensure!(status == "active", Code::AccountDisabled);
        return link_identity(user).await;
    }
    ensure!(identity.verified, Code::IdentityEmailRequired);
    ensure!(app.config().auth.registration, Code::RegistrationClosed);
    let (user, _) = auth::create_account(app, &email, identity.name.as_deref(), "!")
        .await?
        .context(Code::AccountExists)?;
    link_identity(user).await
}

#[derive(Deserialize)]
pub struct Exchange {
    code: String,
}

/// `POST /auth/oauth/exchange`: a session for a finished sign-in, once.
pub async fn exchange(
    State(app): State<App>,
    headers: HeaderMap,
    Json(exchange): Json<Exchange>,
) -> ApiResult<impl IntoResponse> {
    let user: Uuid = sqlx::query_scalar(
        "DELETE FROM login_codes WHERE code_hash=$1 AND expires_at>now() RETURNING user_id",
    )
    .bind(hash_token(&exchange.code))
    .fetch_optional(&app.store.pool)
    .await?
    .ok_or(Code::Unauthorized)?;
    let orgs = user_orgs(&app.store, user).await?;
    auth::sign_in(&app.store, &headers, user, orgs.first().map(|(id, _)| *id)).await
}

/// `GET /me/identities`: the accounts the user signs in with besides a password.
pub async fn identities(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let rows: Vec<(String, String, Option<String>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT provider,subject,email,created_at FROM user_identities WHERE user_id=$1 ORDER BY created_at",
    )
    .bind(auth.user_id)
    .fetch_all(&app.store.pool)
    .await?;
    let password: bool =
        sqlx::query_scalar("SELECT password_hash LIKE '$%' FROM users WHERE id=$1")
            .bind(auth.user_id)
            .fetch_one(&app.store.pool)
            .await?;
    Ok(Json(json!({
        "password": password,
        "providers": providers(&app),
        "identities": rows.into_iter().map(|(provider, subject, email, created)| json!({
            "provider": provider, "subject": subject, "email": email,
            "created_at_ms": created.timestamp_millis(),
        })).collect::<Vec<_>>(),
    })))
}

/// `DELETE /me/identities/{provider}/{subject}`: stop signing in with it. The last way in
/// stays.
pub async fn unlink(
    State(app): State<App>,
    headers: HeaderMap,
    Path((provider, subject)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let auth = authenticate(&app, &headers).await?;
    let mut tx = app.store.pool.begin().await?;
    let (password, identities): (bool, i64) = sqlx::query_as(
        "SELECT u.password_hash LIKE '$%',(SELECT count(*) FROM user_identities WHERE user_id=u.id)
         FROM users u WHERE u.id=$1 FOR UPDATE",
    )
    .bind(auth.user_id)
    .fetch_one(&mut *tx)
    .await?;
    require!(password || identities > 1, Code::LastSignInMethod);
    let removed =
        sqlx::query("DELETE FROM user_identities WHERE user_id=$1 AND provider=$2 AND subject=$3")
            .bind(auth.user_id)
            .bind(&provider)
            .bind(&subject)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    tx.commit().await?;
    require!(removed == 1, Code::NotFoundIdentity);
    Ok(Json(
        json!({"removed": {"provider": provider, "subject": subject}}),
    ))
}
