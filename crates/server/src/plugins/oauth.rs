//! Connecting a member's account to a plugin's server (MCP authorization: OAuth 2.1).
//!
//! The server's 401 names its protected resource metadata (else it is at the well-known
//! address); that names the authorization server, whose metadata must state the same issuer.
//! The platform registers with a client ID metadata document where the server supports one
//! (its client id is the document's URL) and dynamically otherwise. Authorization uses PKCE,
//! a state and the resource; the issuer is checked on return. Every request goes through the
//! same checks as plugin fetches (HTTPS, public addresses), and tokens are kept in the vault
//! and sent only to the server they were issued for.
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Query, State},
    response::{IntoResponse, Redirect, Response},
};
use base64::Engine as _;
use reqwest::{Method, header::HeaderMap};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use super::{client, fetcher};
use crate::{app::App, error::Code};

/// How long a member has to finish authorizing.
const FLOW_MINUTES: i64 = 10;
/// Largest metadata or token answer read.
const MAX_ANSWER: usize = 256 * 1024;

fn callback_url(app: &App) -> String {
    format!(
        "{}/plugins/oauth/callback",
        app.config.public_url.trim_end_matches('/')
    )
}

fn document_url(app: &App) -> String {
    format!(
        "{}/plugins/oauth/client.json",
        app.config.public_url.trim_end_matches('/')
    )
}

/// `GET /plugins/oauth/client.json`: the platform as an OAuth client (client ID metadata
/// document); its URL is the client id.
pub async fn client_document(State(app): State<App>) -> Json<Value> {
    Json(json!({
        "client_id": document_url(&app),
        "client_name": "RyzHub",
        "client_uri": app.config.plugins.client_url,
        "redirect_uris": [callback_url(&app)],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    }))
}

async fn get_json(app: &App, url: &str) -> anyhow::Result<Value> {
    let body = fetcher(app)
        .send(Method::GET, url, HeaderMap::new(), None, MAX_ANSWER)
        .await?;
    serde_json::from_slice(&body).context(Code::PluginOAuthFailed)
}

/// `https://host/a/b` + `.well-known/x` → `https://host/.well-known/x/a/b`.
fn well_known(url: &Url, name: &str) -> String {
    let path = url.path().trim_end_matches('/');
    format!(
        "{}://{}/.well-known/{name}{path}",
        url.scheme(),
        url.host_str().unwrap_or_default().to_owned()
            + &url.port().map(|p| format!(":{p}")).unwrap_or_default()
    )
}

/// The authorization server's metadata, which must be its own (same issuer).
async fn server_metadata(app: &App, issuer: &str) -> anyhow::Result<Value> {
    let parsed = Url::parse(issuer).context(Code::PluginOAuthFailed)?;
    let mut candidates = vec![
        well_known(&parsed, "oauth-authorization-server"),
        well_known(&parsed, "openid-configuration"),
    ];
    candidates.push(format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    ));
    for candidate in candidates {
        if let Ok(metadata) = get_json(app, &candidate).await
            && metadata["issuer"].as_str().map(|i| i.trim_end_matches('/'))
                == Some(issuer.trim_end_matches('/'))
        {
            return Ok(metadata);
        }
    }
    anyhow::bail!(Code::PluginOAuthFailed)
}

fn random_text(bytes: usize) -> String {
    let random: Vec<u8> = (0..bytes).map(|_| rand::random::<u8>()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
}

/// Begin connecting the member's account for `server` of `installation`; the URL to send the
/// browser to.
pub async fn start(
    app: &App,
    org: Uuid,
    user: Uuid,
    installation: Uuid,
    server: &str,
) -> anyhow::Result<String> {
    let installed = super::installed(app, org, installation).await?;
    let server = installed
        .manifest
        .servers
        .iter()
        .find(|s| s.name == server && s.enabled)
        .context(Code::PluginNotFound)?;
    // The server says where its authorization is described when it refuses us.
    let challenge = match super::open(app, org, &installed, server).await {
        Err(error) => match error.downcast::<client::AuthRequired>() {
            Ok(challenge) => challenge,
            Err(error) => return Err(error),
        },
        Ok(_) => anyhow::bail!(Code::PluginAuthNotRequired),
    };
    let server_url = &server.url;
    let resource_url = Url::parse(server_url).context(Code::PluginOAuthFailed)?;
    let resource_metadata = match &challenge.resource_metadata {
        Some(url) => get_json(app, url).await?,
        None => match get_json(app, &well_known(&resource_url, "oauth-protected-resource")).await {
            Ok(metadata) => metadata,
            Err(_) => {
                let origin = resource_url.origin().ascii_serialization();
                get_json(
                    app,
                    &format!("{origin}/.well-known/oauth-protected-resource"),
                )
                .await?
            }
        },
    };
    let issuer = resource_metadata["authorization_servers"][0]
        .as_str()
        .context(Code::PluginOAuthFailed)?
        .to_owned();
    let resource = resource_metadata["resource"]
        .as_str()
        .unwrap_or(server_url)
        .to_owned();
    let metadata = server_metadata(app, &issuer).await?;
    let authorize = metadata["authorization_endpoint"]
        .as_str()
        .context(Code::PluginOAuthFailed)?;
    let token_endpoint = metadata["token_endpoint"]
        .as_str()
        .context(Code::PluginOAuthFailed)?;
    // The browser goes there; it must be an address the platform would use itself.
    let authorize_url = Url::parse(authorize).context(Code::PluginOAuthFailed)?;
    fetcher(app).check(&authorize_url).await?;
    fetcher(app)
        .check(&Url::parse(token_endpoint).context(Code::PluginOAuthFailed)?)
        .await?;
    ensure!(
        metadata["code_challenge_methods_supported"]
            .as_array()
            .is_none_or(|m| m.iter().any(|m| m == "S256")),
        Code::PluginOAuthFailed
    );
    let redirect = callback_url(app);
    let (client_id, client_secret) = if metadata["client_id_metadata_document_supported"] == true {
        (document_url(app), None)
    } else if let Some(registration) = metadata["registration_endpoint"].as_str() {
        let body = json!({
            "client_name": "RyzHub",
            "redirect_uris": [redirect],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        });
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse()?);
        let registered = fetcher(app)
            .send(
                Method::POST,
                registration,
                headers,
                Some(body.to_string().into()),
                MAX_ANSWER,
            )
            .await?;
        let registered: Value =
            serde_json::from_slice(&registered).context(Code::PluginOAuthFailed)?;
        (
            registered["client_id"]
                .as_str()
                .context(Code::PluginOAuthFailed)?
                .to_owned(),
            registered["client_secret"].as_str().map(str::to_owned),
        )
    } else {
        anyhow::bail!(Code::PluginOAuthFailed)
    };
    let scopes = challenge.scope.clone().or_else(|| {
        resource_metadata["scopes_supported"]
            .as_array()
            .map(|scopes| {
                scopes
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
    });
    let verifier = random_text(48);
    let challenge_code = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let state = random_text(24);
    let mut url = authorize_url;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("code_challenge", &challenge_code)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("resource", &resource);
        if let Some(scopes) = scopes.as_deref().filter(|s| !s.is_empty()) {
            query.append_pair("scope", scopes);
        }
    }
    let client = json!({
        "client_id": client_id,
        "client_secret": client_secret,
        "token_endpoint": token_endpoint,
        "resource": resource,
        "redirect_uri": redirect,
        "iss_required": metadata["authorization_response_iss_parameter_supported"] == true,
    });
    sqlx::query(
        "INSERT INTO oauth_flows(state,org_id,user_id,installation_id,server,issuer,verifier,client,expires_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,now()+($9::bigint*interval '1 minute'))",
    )
    .bind(&state)
    .bind(org)
    .bind(user)
    .bind(installation)
    .bind(&server.name)
    .bind(metadata["issuer"].as_str().unwrap_or(&issuer))
    .bind(&verifier)
    .bind(&client)
    .bind(FLOW_MINUTES)
    .execute(&app.store.pool)
    .await?;
    Ok(url.into())
}

#[derive(Deserialize)]
pub struct Returned {
    code: Option<String>,
    state: Option<String>,
    iss: Option<String>,
    error: Option<String>,
}

/// `GET /plugins/oauth/callback`: the browser back from the authorization server.
pub async fn callback(State(app): State<App>, Query(returned): Query<Returned>) -> Response {
    let outcome = finish(&app, returned).await;
    let base = app.config.plugins.client_url.trim_end_matches('/');
    let target = match outcome {
        Ok(installation) => format!("{base}/#/plugins?connected={installation}"),
        Err(error) => {
            tracing::info!(error = format!("{error:#}"), "plugin account not connected");
            format!(
                "{base}/#/plugins?connect_error={}",
                crate::error::public_code(&error).as_str()
            )
        }
    };
    Redirect::to(&target).into_response()
}

async fn finish(app: &App, returned: Returned) -> anyhow::Result<Uuid> {
    let state = returned.state.context(Code::PluginOAuthFailed)?;
    type Flow = (Uuid, Uuid, Uuid, String, String, String, Value);
    // The state is spent whatever happens next.
    let flow: Flow = sqlx::query_as(
        "DELETE FROM oauth_flows WHERE state=$1 AND expires_at>now()
         RETURNING org_id,user_id,installation_id,server,issuer,verifier,client",
    )
    .bind(&state)
    .fetch_optional(&app.store.pool)
    .await?
    .context(Code::PluginOAuthFailed)?;
    let (org, user, installation, server, issuer, verifier, client) = flow;
    ensure!(returned.error.is_none(), Code::PluginOAuthFailed);
    // Mix-up defense: the answer must come from the server we sent the member to.
    match returned.iss.as_deref() {
        Some(iss) => ensure!(iss == issuer, Code::PluginOAuthFailed),
        None => ensure!(client["iss_required"] != true, Code::PluginOAuthFailed),
    }
    let code = returned.code.context(Code::PluginOAuthFailed)?;
    let mut form = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code),
        (
            "redirect_uri",
            client["redirect_uri"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        ),
        (
            "client_id",
            client["client_id"].as_str().unwrap_or_default().to_owned(),
        ),
        ("code_verifier", verifier),
        (
            "resource",
            client["resource"].as_str().unwrap_or_default().to_owned(),
        ),
    ];
    if let Some(secret) = client["client_secret"].as_str() {
        form.push(("client_secret", secret.to_owned()));
    }
    let tokens = token_request(app, &client, &form).await?;
    store(
        app,
        org,
        user,
        installation,
        &server,
        &issuer,
        &client,
        &tokens,
    )
    .await?;
    // The first tools a member sees after connecting are the ones they installed for.
    let mut tx = app.store.tx(org).await?;
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM plugin_tools WHERE org_id=$1 AND installation_id=$2)",
    )
    .bind(org)
    .bind(installation)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    super::refresh_tools(app, org, installation, !known).await?;
    Ok(installation)
}

async fn token_request(
    app: &App,
    client: &Value,
    form: &[(&str, String)],
) -> anyhow::Result<Value> {
    let endpoint = client["token_endpoint"]
        .as_str()
        .context(Code::PluginOAuthFailed)?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().map(|(k, v)| (*k, v.as_str())))
        .finish();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/x-www-form-urlencoded".parse()?);
    headers.insert("accept", "application/json".parse()?);
    let tokens = fetcher(app)
        .send(
            Method::POST,
            endpoint,
            headers,
            Some(body.into()),
            MAX_ANSWER,
        )
        .await?;
    let tokens: Value = serde_json::from_slice(&tokens).context(Code::PluginOAuthFailed)?;
    ensure!(tokens["access_token"].is_string(), Code::PluginOAuthFailed);
    Ok(tokens)
}

#[allow(clippy::too_many_arguments)]
async fn store(
    app: &App,
    org: Uuid,
    user: Uuid,
    installation: Uuid,
    server: &str,
    issuer: &str,
    client: &Value,
    tokens: &Value,
) -> anyhow::Result<()> {
    let mut tx = app.store.tx(org).await?;
    let old: Option<Uuid> = sqlx::query_scalar(
        "SELECT token_secret FROM plugin_connections WHERE org_id=$1 AND installation_id=$2 AND user_id=$3 AND server=$4",
    )
    .bind(org)
    .bind(installation)
    .bind(user)
    .bind(server)
    .fetch_optional(&mut *tx)
    .await?;
    let kept = json!({
        "access_token": tokens["access_token"],
        "refresh_token": tokens["refresh_token"],
        "client_secret": client["client_secret"],
    });
    let purpose = format!("plugin-oauth:{installation}:{server}");
    let secret = crate::vault::put(
        &mut tx,
        &app.config.llm,
        org,
        &purpose,
        kept.to_string().as_bytes(),
    )
    .await?;
    let expires = tokens["expires_in"]
        .as_i64()
        .map(|seconds| chrono::Utc::now() + chrono::Duration::seconds(seconds));
    let public_client = json!({
        "client_id": client["client_id"],
        "token_endpoint": client["token_endpoint"],
        "resource": client["resource"],
    });
    sqlx::query(
        "INSERT INTO plugin_connections(org_id,installation_id,user_id,server,issuer,client,token_secret,expires_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8)
         ON CONFLICT(installation_id,user_id,server) DO UPDATE SET issuer=$5,client=$6,token_secret=$7,
           expires_at=$8,connected_at=now()",
    )
    .bind(org)
    .bind(installation)
    .bind(user)
    .bind(server)
    .bind(issuer)
    .bind(public_client)
    .bind(secret)
    .bind(expires)
    .execute(&mut *tx)
    .await?;
    if let Some(old) = old {
        crate::vault::delete(&mut tx, org, old).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The member's access token for `server`, refreshed when it is about to expire.
pub async fn token(
    app: &App,
    org: Uuid,
    user: Uuid,
    installation: Uuid,
    server: &str,
) -> anyhow::Result<Option<String>> {
    let mut tx = app.store.tx(org).await?;
    let connection: Option<(String, Value, Uuid, Option<chrono::DateTime<chrono::Utc>>)> =
        sqlx::query_as(
            "SELECT issuer,client,token_secret,expires_at FROM plugin_connections
             WHERE org_id=$1 AND installation_id=$2 AND user_id=$3 AND server=$4",
        )
        .bind(org)
        .bind(installation)
        .bind(user)
        .bind(server)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((issuer, client, secret, expires)) = connection else {
        return Ok(None);
    };
    let purpose = format!("plugin-oauth:{installation}:{server}");
    let kept: Value = serde_json::from_slice(
        &crate::vault::get(&mut tx, &app.config.llm, org, secret, &purpose).await?,
    )?;
    tx.commit().await?;
    let fresh = expires.is_none_or(|at| at > chrono::Utc::now() + chrono::Duration::seconds(60));
    if fresh {
        return Ok(kept["access_token"].as_str().map(str::to_owned));
    }
    let Some(refresh) = kept["refresh_token"].as_str() else {
        return Ok(kept["access_token"].as_str().map(str::to_owned));
    };
    let mut form = vec![
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", refresh.to_owned()),
        (
            "client_id",
            client["client_id"].as_str().unwrap_or_default().to_owned(),
        ),
        (
            "resource",
            client["resource"].as_str().unwrap_or_default().to_owned(),
        ),
    ];
    if let Some(secret) = kept["client_secret"].as_str() {
        form.push(("client_secret", secret.to_owned()));
    }
    let mut tokens = token_request(app, &client, &form).await?;
    if tokens["refresh_token"].is_null() {
        tokens["refresh_token"] = json!(refresh);
    }
    let mut full_client = client.clone();
    full_client["client_secret"] = kept["client_secret"].clone();
    store(
        app,
        org,
        user,
        installation,
        server,
        &issuer,
        &full_client,
        &tokens,
    )
    .await?;
    Ok(tokens["access_token"].as_str().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_addresses_insert_before_the_path() {
        let url = Url::parse("https://auth.example.com/tenant/x").unwrap();
        assert_eq!(
            well_known(&url, "oauth-authorization-server"),
            "https://auth.example.com/.well-known/oauth-authorization-server/tenant/x"
        );
        let root = Url::parse("http://127.0.0.1:9000").unwrap();
        assert_eq!(
            well_known(&root, "oauth-protected-resource"),
            "http://127.0.0.1:9000/.well-known/oauth-protected-resource"
        );
    }
}
