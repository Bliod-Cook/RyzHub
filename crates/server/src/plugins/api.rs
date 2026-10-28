//! Plugin management for members: preview a source, install it after confirming what it asks
//! for, change its settings, connect accounts, update, confirm changes, turn it on or off per
//! project and conversation. Instance administrators stop plugins for everyone.
use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    Source, client, fetcher, obtain,
    package::{ConfigField, Manifest},
    refresh_tools,
};
use crate::{
    app::App,
    error::{ApiResult, Code},
    http::member_tx,
    store::{Tx, effective_policy},
};
use ryzhub_domain::OrgContext;

#[derive(Deserialize)]
pub struct PreviewRequest {
    source: Source,
    /// An upload's archive, base64.
    #[serde(default)]
    archive: Option<String>,
}

/// `POST /orgs/{org}/plugins/preview`: fetch and read a source; what installing it means.
pub async fn preview(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(request): Json<PreviewRequest>,
) -> ApiResult<Json<Value>> {
    ensure_enabled(&app)?;
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let policy = effective_policy(&mut tx, &app.config().policy, org).await?;
    tx.commit().await?;
    require!(
        policy.allow_personal_plugins != Some(false),
        Code::PolicyDenied
    );
    require!(
        policy
            .plugin_sources
            .as_ref()
            .is_none_or(|kinds| kinds.contains(request.source.kind())),
        Code::PolicyDenied
    );
    let upload = request
        .archive
        .as_deref()
        .map(|archive| base64::engine::general_purpose::STANDARD.decode(archive))
        .transpose()
        .context(Code::InvalidInput)?;
    let obtained = obtain(&app, &request.source, upload.as_deref()).await?;
    require!(
        obtained.trust != "unverified" || policy.allow_unverified_plugins != Some(false),
        Code::PluginUnverifiedDenied
    );
    let mut tx = app.store.tx(org).await?;
    require!(
        !super::blocked(&mut tx, &obtained.manifest.name, &obtained.content_hash).await?,
        Code::PluginBlocked
    );
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT i.id FROM plugin_installations i JOIN plugins p ON p.org_id=i.org_id AND p.id=i.plugin_id
         WHERE i.org_id=$1 AND i.user_id=$2 AND p.name=$3",
    )
    .bind(org)
    .bind(ctx.user_id)
    .bind(&obtained.manifest.name)
    .fetch_optional(&mut *tx)
    .await?;
    let plugin = Uuid::now_v7();
    let version = Uuid::now_v7();
    sqlx::query("INSERT INTO plugins(id,org_id,name,source,created_by) VALUES($1,$2,$3,$4,$5)")
        .bind(plugin)
        .bind(org)
        .bind(&obtained.manifest.name)
        .bind(serde_json::to_value(&request.source)?)
        .bind(ctx.user_id)
        .execute(&mut *tx)
        .await?;
    insert_version(&mut tx, org, plugin, version, &obtained).await?;
    tx.commit().await?;
    // What its servers offer now, as far as they say without the member's settings.
    let mut tools = vec![];
    let mut unlisted = vec![];
    for server in obtained.manifest.servers.iter().filter(|s| s.enabled) {
        let headers = super::package_headers(server)?;
        let listed = async {
            let session = client::Session::open(&fetcher(&app), &server.url, headers).await?;
            session.tools().await
        };
        match tokio::time::timeout(std::time::Duration::from_secs(20), listed).await {
            Ok(Ok(listed)) => tools.extend(listed.into_iter().map(|tool| {
                json!({"server": server.name, "name": tool["name"], "description": tool["description"],
                       "read_only": tool["annotations"]["readOnlyHint"] == true})
            })),
            Ok(Err(error)) if error.downcast_ref::<client::AuthRequired>().is_some() => {
                unlisted.push(json!({"server": server.name, "reason": "needs_connection"}))
            }
            _ => unlisted.push(json!({"server": server.name, "reason": "unreachable"})),
        }
    }
    let mut preview = describe(&obtained.manifest);
    preview["plugin_id"] = json!(plugin);
    preview["version_id"] = json!(version);
    preview["trust"] = json!(obtained.trust);
    preview["content_hash"] = json!(obtained.content_hash);
    preview["commit"] = json!(obtained.commit);
    preview["tools"] = json!(tools);
    preview["unlisted"] = json!(unlisted);
    preview["existing"] = json!(existing);
    preview["source"] = serde_json::to_value(&request.source)?;
    Ok(Json(preview))
}

fn ensure_enabled(app: &App) -> anyhow::Result<()> {
    ensure!(app.config().plugins.enabled, Code::PluginsDisabled);
    Ok(())
}

async fn insert_version(
    tx: &mut Tx<'_>,
    org: Uuid,
    plugin: Uuid,
    version: Uuid,
    obtained: &super::Obtained,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO plugin_versions(id,org_id,plugin_id,version,content_hash,trust,manifest,commit)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(version)
    .bind(org)
    .bind(plugin)
    .bind(&obtained.manifest.version)
    .bind(&obtained.content_hash)
    .bind(obtained.trust)
    .bind(serde_json::to_value(&obtained.manifest)?)
    .bind(&obtained.commit)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Everything a member confirms, in full: servers, permissions, settings, and the text of the
/// skills and commands, which are prompts.
fn describe(manifest: &Manifest) -> Value {
    json!({
        "name": manifest.name,
        "version": manifest.version,
        "description": manifest.description,
        "author": manifest.author,
        "homepage": manifest.homepage,
        "format": manifest.format,
        "servers": manifest.servers.iter().map(|s| json!({
            "name": s.name, "kind": s.kind, "url": s.url, "enabled": s.enabled,
            "headers": s.headers.keys().collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "skills": manifest.skills.iter().map(|s| json!({"name": s.name, "description": s.description, "body": s.body})).collect::<Vec<_>>(),
        "commands": manifest.commands.iter().map(|c| json!({"name": c.name, "title": c.title, "description": c.description, "prompt": c.prompt, "params": c.params})).collect::<Vec<_>>(),
        "permissions": manifest.permissions,
        "ui": manifest.ui,
        "user_config": manifest.user_config,
        "warnings": manifest.warnings,
    })
}

#[derive(Deserialize)]
pub struct InstallRequest {
    version_id: Uuid,
    #[serde(default)]
    config: Map<String, Value>,
    #[serde(default)]
    secrets: Map<String, Value>,
}

/// `POST /orgs/{org}/plugins`: install a previewed version, confirming what it asks for.
pub async fn install(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
    Json(request): Json<InstallRequest>,
) -> ApiResult<Json<Value>> {
    ensure_enabled(&app)?;
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let (plugin, name, manifest, hash, creator): (Uuid, String, Value, String, Uuid) =
        sqlx::query_as(
            "SELECT p.id,p.name,v.manifest,v.content_hash,p.created_by FROM plugin_versions v
         JOIN plugins p ON p.org_id=v.org_id AND p.id=v.plugin_id WHERE v.org_id=$1 AND v.id=$2",
        )
        .bind(org)
        .bind(request.version_id)
        .fetch_optional(&mut *tx)
        .await?
        .context(Code::PluginNotFound)?;
    require!(creator == ctx.user_id, Code::PluginNotFound);
    require!(
        !super::blocked(&mut tx, &name, &hash).await?,
        Code::PluginBlocked
    );
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM plugin_installations i JOIN plugins p ON p.org_id=i.org_id AND p.id=i.plugin_id
         WHERE i.org_id=$1 AND i.user_id=$2 AND p.name=$3)",
    )
    .bind(org)
    .bind(ctx.user_id)
    .bind(&name)
    .fetch_one(&mut *tx)
    .await?;
    require!(!taken, Code::PluginExists);
    let manifest: Manifest = serde_json::from_value(manifest)?;
    let installation = Uuid::now_v7();
    let (config, secrets) = settings(
        &mut tx,
        &app,
        org,
        installation,
        &manifest.user_config,
        &Map::new(),
        &Map::new(),
        &request.config,
        &request.secrets,
    )
    .await?;
    // TODO(org): organization-wide installations (`scope='org'`) need their management UI;
    // every installation is a member's, the column's default.
    sqlx::query(
        "INSERT INTO plugin_installations(id,org_id,plugin_id,user_id,version_id,state,config,secrets,confirmed_at)
         VALUES($1,$2,$3,$4,$5,'active',$6,$7,now())",
    )
    .bind(installation)
    .bind(org)
    .bind(plugin)
    .bind(ctx.user_id)
    .bind(request.version_id)
    .bind(Value::Object(config))
    .bind(Value::Object(secrets))
    .execute(&mut *tx)
    .await?;
    crate::store::audit(
        &mut tx,
        &ctx,
        "plugin.install",
        &installation.to_string(),
        &name,
    )
    .await?;
    tx.commit().await?;
    let listed = refresh_tools(&app, org, installation, true).await?;
    let mut entry = entry(&app, &ctx, installation).await?;
    entry["listed"] = serde_json::to_value(listed)?;
    Ok(Json(entry))
}

/// Checked settings: plain values in `config`, secret ones in the vault. `old_*` are the
/// current values; a setting left out keeps its value, a null clears it.
#[allow(clippy::too_many_arguments)]
async fn settings(
    tx: &mut Tx<'_>,
    app: &App,
    org: Uuid,
    installation: Uuid,
    fields: &std::collections::BTreeMap<String, ConfigField>,
    old_config: &Map<String, Value>,
    old_secrets: &Map<String, Value>,
    config: &Map<String, Value>,
    secrets: &Map<String, Value>,
) -> anyhow::Result<(Map<String, Value>, Map<String, Value>)> {
    let plain = config.keys().map(|name| (name, false));
    for (name, secret) in plain.chain(secrets.keys().map(|name| (name, true))) {
        ensure!(
            fields.get(name).is_some_and(|field| field.secret == secret),
            Code::InvalidArguments
        );
    }
    let mut kept_config = old_config.clone();
    let mut kept_secrets = old_secrets.clone();
    for (name, field) in fields {
        let given = if field.secret {
            secrets.get(name)
        } else {
            config.get(name)
        };
        match given {
            None => {}
            Some(Value::Null) => {
                kept_config.remove(name);
                if let Some(id) = kept_secrets
                    .remove(name)
                    .and_then(|v| v.as_str().and_then(|v| v.parse().ok()))
                {
                    crate::vault::delete(tx, org, id).await?;
                }
            }
            Some(value) => {
                let fits = match field.kind.as_str() {
                    "number" => value.is_number(),
                    "boolean" => value.is_boolean(),
                    _ => value.as_str().is_some_and(|text| text.len() <= 8192),
                };
                ensure!(fits, Code::InvalidArguments);
                if field.secret {
                    let text = value.as_str().context(Code::InvalidArguments)?;
                    let purpose = format!("plugin:{installation}:{name}");
                    let id =
                        crate::vault::put(tx, &app.config().llm, org, &purpose, text.as_bytes())
                            .await?;
                    if let Some(old) = kept_secrets
                        .insert(name.clone(), json!(id))
                        .and_then(|v| v.as_str().and_then(|v| v.parse().ok()))
                    {
                        crate::vault::delete(tx, org, old).await?;
                    }
                } else {
                    kept_config.insert(name.clone(), value.clone());
                }
            }
        }
        let set = if field.secret {
            kept_secrets.contains_key(name)
        } else {
            kept_config.contains_key(name) || !field.default.is_null()
        };
        ensure!(!field.required || set, Code::PluginSettingRequired);
    }
    Ok((kept_config, kept_secrets))
}

/// An installation as its member sees it.
async fn entry(app: &App, ctx: &OrgContext, installation: Uuid) -> anyhow::Result<Value> {
    let mut tx = app.store.tx(ctx.org_id).await?;
    let entries = list_in(&mut tx, ctx, Some(installation)).await?;
    tx.commit().await?;
    entries.into_iter().next().context(Code::PluginNotFound)
}

async fn list_in(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
    only: Option<Uuid>,
) -> anyhow::Result<Vec<Value>> {
    type Row = (
        Uuid,
        Uuid,
        String,
        Value,
        String,
        bool,
        Value,
        Value,
        Value,
        String,
        String,
        Option<Uuid>,
        bool,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT i.id,i.plugin_id,p.name,p.source,i.state,i.enabled,i.config,i.secrets,v.manifest,v.trust,v.content_hash,
                i.pending_version_id,
                EXISTS(SELECT 1 FROM plugin_blocks b WHERE b.name=p.name OR b.content_hash=v.content_hash)
         FROM plugin_installations i JOIN plugins p ON p.org_id=i.org_id AND p.id=i.plugin_id
         JOIN plugin_versions v ON v.org_id=i.org_id AND v.id=i.version_id
         WHERE i.org_id=$1 AND i.user_id=$2 AND ($3::uuid IS NULL OR i.id=$3)
         ORDER BY p.name",
    )
    .bind(ctx.org_id)
    .bind(ctx.user_id)
    .bind(only)
    .fetch_all(&mut **tx)
    .await?;
    let mut entries = vec![];
    for (
        id,
        plugin,
        name,
        source,
        state,
        enabled,
        config,
        secrets,
        manifest,
        trust,
        hash,
        pending,
        blocked,
    ) in rows
    {
        let manifest: Manifest = serde_json::from_value(manifest)?;
        let tools: Vec<(String, String, Value, String, Option<String>)> = sqlx::query_as(
            "SELECT server,tool,definition,digest,approved_digest FROM plugin_tools
             WHERE org_id=$1 AND installation_id=$2 ORDER BY server,tool",
        )
        .bind(ctx.org_id)
        .bind(id)
        .fetch_all(&mut **tx)
        .await?;
        let connected: Vec<String> = sqlx::query_scalar(
            "SELECT server FROM plugin_connections WHERE org_id=$1 AND installation_id=$2 AND user_id=$3",
        )
        .bind(ctx.org_id)
        .bind(id)
        .bind(ctx.user_id)
        .fetch_all(&mut **tx)
        .await?;
        let pending_version = match pending {
            Some(version) => {
                let (manifest, trust): (Value, String) = sqlx::query_as(
                    "SELECT manifest,trust FROM plugin_versions WHERE org_id=$1 AND id=$2",
                )
                .bind(ctx.org_id)
                .bind(version)
                .fetch_one(&mut **tx)
                .await?;
                let manifest: Manifest = serde_json::from_value(manifest)?;
                let mut described = describe(&manifest);
                described["version_id"] = json!(version);
                described["trust"] = json!(trust);
                described
            }
            None => Value::Null,
        };
        let mut described = describe(&manifest);
        described["id"] = json!(id);
        described["plugin_id"] = json!(plugin);
        described["source"] = source;
        described["state"] = json!(state);
        described["enabled"] = json!(enabled);
        described["trust"] = json!(trust);
        described["content_hash"] = json!(hash);
        described["blocked"] = json!(blocked);
        described["config"] = config;
        described["secrets_set"] = json!(
            secrets
                .as_object()
                .map(|s| s.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        );
        described["connected"] = json!(connected);
        described["pending_version"] = pending_version;
        described["tools"] = json!(
            tools
                .iter()
                .map(|(server, tool, definition, digest, approved)| json!({
                    "name": super::tool_name(&name, tool),
                    "server": server,
                    "tool": tool,
                    "description": definition["description"],
                    "read_only": definition["annotations"]["readOnlyHint"] == true,
                    "confirmed": approved.as_deref() == Some(digest.as_str()),
                    "new": approved.is_none(),
                }))
                .collect::<Vec<_>>()
        );
        entries.push(described);
    }
    Ok(entries)
}

/// `GET /orgs/{org}/plugins`: the member's plugins.
pub async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let entries = list_in(&mut tx, &ctx, None).await?;
    tx.commit().await?;
    Ok(Json(
        json!({"plugins": entries, "enabled": app.config().plugins.enabled}),
    ))
}

/// `GET /orgs/{org}/plugins/catalog`: the official catalog.
pub async fn catalog(
    State(app): State<App>,
    headers: HeaderMap,
    Path(org): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let (_, tx) = member_tx(&app, &headers, org).await?;
    tx.commit().await?;
    Ok(Json(json!({"plugins": super::catalog(&app).await?})))
}

/// The installation `id`, which must be the caller's.
async fn owned(
    tx: &mut Tx<'_>,
    ctx: &OrgContext,
    id: Uuid,
) -> anyhow::Result<(Uuid, Value, Value, Value)> {
    sqlx::query_as(
        "SELECT i.version_id,v.manifest,i.config,i.secrets FROM plugin_installations i
         JOIN plugin_versions v ON v.org_id=i.org_id AND v.id=i.version_id
         WHERE i.org_id=$1 AND i.id=$2 AND i.user_id=$3 FOR UPDATE OF i",
    )
    .bind(ctx.org_id)
    .bind(id)
    .bind(ctx.user_id)
    .fetch_optional(&mut **tx)
    .await?
    .context(Code::PluginNotFound)
}

#[derive(Deserialize)]
pub struct ChangeRequest {
    enabled: Option<bool>,
    #[serde(default)]
    config: Map<String, Value>,
    #[serde(default)]
    secrets: Map<String, Value>,
}

/// `PATCH /orgs/{org}/plugins/{id}`: turn on or off, change settings.
pub async fn change(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
    Json(request): Json<ChangeRequest>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let (_, manifest, config, secrets) = owned(&mut tx, &ctx, id).await?;
    let manifest: Manifest = serde_json::from_value(manifest)?;
    let (config, secrets) = settings(
        &mut tx,
        &app,
        org,
        id,
        &manifest.user_config,
        config.as_object().context("config")?,
        secrets.as_object().context("secrets")?,
        &request.config,
        &request.secrets,
    )
    .await?;
    sqlx::query(
        "UPDATE plugin_installations SET enabled=coalesce($3,enabled),config=$4,secrets=$5,updated_at=now()
         WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .bind(request.enabled)
    .bind(Value::Object(config))
    .bind(Value::Object(secrets))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(entry(&app, &ctx, id).await?))
}

/// `DELETE /orgs/{org}/plugins/{id}`: uninstall; its secrets and accounts go with it.
pub async fn remove(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let (_, _, _, secrets) = owned(&mut tx, &ctx, id).await?;
    let tokens: Vec<Uuid> = sqlx::query_scalar(
        "SELECT token_secret FROM plugin_connections WHERE org_id=$1 AND installation_id=$2",
    )
    .bind(org)
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let plugin: Uuid = sqlx::query_scalar(
        "DELETE FROM plugin_installations WHERE org_id=$1 AND id=$2 RETURNING plugin_id",
    )
    .bind(org)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    for secret in secrets
        .as_object()
        .into_iter()
        .flat_map(|s| s.values())
        .filter_map(|v| v.as_str()?.parse().ok())
        .chain(tokens)
    {
        crate::vault::delete(&mut tx, org, secret).await?;
    }
    sqlx::query("DELETE FROM plugins WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(plugin)
        .execute(&mut *tx)
        .await?;
    crate::store::audit(&mut tx, &ctx, "plugin.remove", &id.to_string(), "").await?;
    tx.commit().await?;
    Ok(Json(json!({"removed": id})))
}

/// `POST /orgs/{org}/plugins/{id}/update`: fetch the source again. A version that asks for
/// more, or whose prompts changed, waits for the member's confirmation.
pub async fn update(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    ensure_enabled(&app)?;
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let (plugin, source, current, hash): (Uuid, Value, Value, String) = sqlx::query_as(
        "SELECT p.id,p.source,v.manifest,v.content_hash FROM plugin_installations i
         JOIN plugins p ON p.org_id=i.org_id AND p.id=i.plugin_id
         JOIN plugin_versions v ON v.org_id=i.org_id AND v.id=i.version_id
         WHERE i.org_id=$1 AND i.id=$2 AND i.user_id=$3",
    )
    .bind(org)
    .bind(id)
    .bind(ctx.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .context(Code::PluginNotFound)?;
    tx.commit().await?;
    let source: Source = serde_json::from_value(source)?;
    require!(
        !matches!(source, Source::Upload { .. }),
        Code::PluginUpdateUnavailable
    );
    let obtained = obtain(&app, &source, None).await?;
    if obtained.content_hash == hash {
        let listed = refresh_tools(&app, org, id, false).await?;
        let mut entry = entry(&app, &ctx, id).await?;
        entry["updated"] = json!(false);
        entry["listed"] = serde_json::to_value(listed)?;
        return Ok(Json(entry));
    }
    let current: Manifest = serde_json::from_value(current)?;
    let prompts_changed = current.skills != obtained.manifest.skills
        || current.commands != obtained.manifest.commands;
    let confirm = current.widened_by(&obtained.manifest) || prompts_changed;
    let mut tx = app.store.tx(org).await?;
    require!(
        !super::blocked(&mut tx, &obtained.manifest.name, &obtained.content_hash).await?,
        Code::PluginBlocked
    );
    let version = Uuid::now_v7();
    insert_version(&mut tx, org, plugin, version, &obtained).await?;
    if confirm {
        sqlx::query(
            "UPDATE plugin_installations SET pending_version_id=$3,state='needs_reconfirm',updated_at=now()
             WHERE org_id=$1 AND id=$2",
        )
        .bind(org)
        .bind(id)
        .bind(version)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE plugin_installations SET version_id=$3,pending_version_id=NULL,updated_at=now()
             WHERE org_id=$1 AND id=$2",
        )
        .bind(org)
        .bind(id)
        .bind(version)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let listed = refresh_tools(&app, org, id, false).await?;
    let mut entry = entry(&app, &ctx, id).await?;
    entry["updated"] = json!(true);
    entry["listed"] = serde_json::to_value(listed)?;
    Ok(Json(entry))
}

/// `POST /orgs/{org}/plugins/{id}/confirm`: accept a waiting version and changed tools.
pub async fn confirm(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    owned(&mut tx, &ctx, id).await?;
    sqlx::query(
        "UPDATE plugin_tools SET approved_digest=digest WHERE org_id=$1 AND installation_id=$2",
    )
    .bind(org)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE plugin_installations SET version_id=coalesce(pending_version_id,version_id),
           pending_version_id=NULL,state='active',confirmed_at=now(),updated_at=now()
         WHERE org_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    crate::store::audit(&mut tx, &ctx, "plugin.confirm", &id.to_string(), "").await?;
    tx.commit().await?;
    // A version confirmed now may list other tools; those the member just saw are confirmed.
    let listed = refresh_tools(&app, org, id, false).await?;
    let mut entry = entry(&app, &ctx, id).await?;
    entry["listed"] = serde_json::to_value(listed)?;
    Ok(Json(entry))
}

/// `POST /orgs/{org}/plugins/{id}/refresh`: list its tools again.
pub async fn refresh(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    owned(&mut tx, &ctx, id).await?;
    tx.commit().await?;
    let listed = refresh_tools(&app, org, id, false).await?;
    let mut entry = entry(&app, &ctx, id).await?;
    entry["listed"] = serde_json::to_value(listed)?;
    Ok(Json(entry))
}

#[derive(Deserialize)]
pub struct ConnectRequest {
    server: String,
}

/// `POST /orgs/{org}/plugins/{id}/connect`: where to send the browser to connect an account.
pub async fn connect(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id)): Path<(Uuid, Uuid)>,
    Json(request): Json<ConnectRequest>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    owned(&mut tx, &ctx, id).await?;
    tx.commit().await?;
    let url = super::oauth::start(&app, org, ctx.user_id, id, &request.server).await?;
    Ok(Json(json!({"url": url})))
}

/// `DELETE /orgs/{org}/plugins/{id}/connections/{server}`: forget the account.
pub async fn disconnect(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, id, server)): Path<(Uuid, Uuid, String)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    owned(&mut tx, &ctx, id).await?;
    let secret: Option<Uuid> = sqlx::query_scalar(
        "DELETE FROM plugin_connections WHERE org_id=$1 AND installation_id=$2 AND user_id=$3 AND server=$4
         RETURNING token_secret",
    )
    .bind(org)
    .bind(id)
    .bind(ctx.user_id)
    .bind(&server)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(secret) = secret {
        crate::vault::delete(&mut tx, org, secret).await?;
    }
    tx.commit().await?;
    Ok(Json(entry(&app, &ctx, id).await?))
}

#[derive(Deserialize)]
pub struct Toggle {
    /// On, off, or null to follow the project (or the plugin's own switch).
    enabled: Option<bool>,
}

/// `PUT /orgs/{org}/conversations/{conv}/plugins/{id}`: a conversation's own switch.
pub async fn conversation_toggle(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv, id)): Path<(Uuid, Uuid, Uuid)>,
    Json(toggle): Json<Toggle>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    owned(&mut tx, &ctx, id).await?;
    let mine: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM conversations WHERE org_id=$1 AND id=$2 AND creator_id=$3)",
    )
    .bind(org)
    .bind(conv)
    .bind(ctx.user_id)
    .fetch_one(&mut *tx)
    .await?;
    require!(mine, Code::Forbidden);
    match toggle.enabled {
        Some(enabled) => sqlx::query(
            "INSERT INTO conversation_plugins(org_id,conversation_id,installation_id,enabled) VALUES($1,$2,$3,$4)
             ON CONFLICT(conversation_id,installation_id) DO UPDATE SET enabled=$4",
        )
        .bind(org)
        .bind(conv)
        .bind(id)
        .bind(enabled),
        None => sqlx::query(
            "DELETE FROM conversation_plugins WHERE org_id=$1 AND conversation_id=$2 AND installation_id=$3",
        )
        .bind(org)
        .bind(conv)
        .bind(id),
    }
    .execute(&mut *tx)
    .await?;
    let states = conversation_states(&mut tx, org, conv).await?;
    tx.commit().await?;
    Ok(Json(states))
}

/// `GET /orgs/{org}/conversations/{conv}/plugins`: which plugins are on there, and why.
pub async fn conversation_plugins(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, conv)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    let mine: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM conversations WHERE org_id=$1 AND id=$2 AND creator_id=$3)",
    )
    .bind(org)
    .bind(conv)
    .bind(ctx.user_id)
    .fetch_one(&mut *tx)
    .await?;
    require!(mine, Code::Forbidden);
    let states = conversation_states(&mut tx, org, conv).await?;
    tx.commit().await?;
    Ok(Json(states))
}

async fn conversation_states(tx: &mut Tx<'_>, org: Uuid, conv: Uuid) -> anyhow::Result<Value> {
    type Row = (Uuid, String, bool, Option<bool>, Option<bool>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT i.id,p.name,i.enabled,cp.enabled,pp.enabled FROM plugin_installations i
         JOIN plugins p ON p.org_id=i.org_id AND p.id=i.plugin_id
         JOIN conversations c ON c.org_id=i.org_id AND c.id=$2
         LEFT JOIN conversation_plugins cp ON cp.conversation_id=c.id AND cp.installation_id=i.id
         LEFT JOIN project_plugins pp ON pp.project_id=c.project_id AND pp.installation_id=i.id
         WHERE i.org_id=$1 AND i.user_id=c.creator_id ORDER BY p.name",
    )
    .bind(org)
    .bind(conv)
    .fetch_all(&mut **tx)
    .await?;
    let commands = super::commands(tx, org, conv).await?;
    Ok(json!({
        "plugins": rows.into_iter().map(|(id, name, own, conversation, project)| json!({
            "id": id, "name": name,
            "enabled": conversation.or(project).unwrap_or(own),
            "conversation": conversation, "project": project, "own": own,
        })).collect::<Vec<_>>(),
        "commands": commands,
    }))
}

/// `PUT /orgs/{org}/projects/{project}/plugins/{id}`: a project's switch for its conversations.
pub async fn project_toggle(
    State(app): State<App>,
    headers: HeaderMap,
    Path((org, project, id)): Path<(Uuid, Uuid, Uuid)>,
    Json(toggle): Json<Toggle>,
) -> ApiResult<Json<Value>> {
    let (ctx, mut tx) = member_tx(&app, &headers, org).await?;
    owned(&mut tx, &ctx, id).await?;
    let mine: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE org_id=$1 AND id=$2 AND creator_id=$3 AND state='active')",
    )
    .bind(org)
    .bind(project)
    .bind(ctx.user_id)
    .fetch_one(&mut *tx)
    .await?;
    require!(mine, Code::Forbidden);
    match toggle.enabled {
        Some(enabled) => sqlx::query(
            "INSERT INTO project_plugins(org_id,project_id,installation_id,enabled) VALUES($1,$2,$3,$4)
             ON CONFLICT(project_id,installation_id) DO UPDATE SET enabled=$4",
        )
        .bind(org)
        .bind(project)
        .bind(id)
        .bind(enabled),
        None => sqlx::query(
            "DELETE FROM project_plugins WHERE org_id=$1 AND project_id=$2 AND installation_id=$3",
        )
        .bind(org)
        .bind(project)
        .bind(id),
    }
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({"project": project, "installation": id, "enabled": toggle.enabled}),
    ))
}
