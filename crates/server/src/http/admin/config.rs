//! `/admin/config`: the instance configuration, read and changed through its file.
use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, header},
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

use super::{admin, audit};
use crate::{
    app::App,
    config::{
        Config, edit, parse_toml,
        reload::{rule_of, settings, version},
    },
    error::{ApiResult, Code, Detail},
};

pub fn routes() -> Router<App> {
    Router::new()
        .route("/admin/config", get(show))
        .route("/admin/config/{section}", put(update))
        .route("/admin/search/reindex", post(reindex))
}

/// `GET /admin/config`: the running configuration, how each setting takes effect and whether
/// the panel may change it, what waits for a restart, and which model key variables are set.
async fn show(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    admin(&app, &headers).await?;
    Ok(Json(view(&app)?))
}

fn view(app: &App) -> anyhow::Result<Value> {
    let config = app.config();
    let status = app.running.status();
    let path = app.running.path.as_deref();
    let editable = path.is_some_and(edit::writable);
    let text = path.map(std::fs::read_to_string).transpose()?;
    let file = text
        .as_deref()
        .and_then(|text| parse_toml::<Config>(text).ok());
    let fields: Map<String, Value> = settings(&config)
        .into_keys()
        .filter_map(|path| {
            let rule = rule_of(&path)?;
            let field = json!({
                "section": rule.section,
                "apply": rule.apply,
                "editable": editable && rule.panel,
            });
            Some((path, field))
        })
        .collect();
    let env: Map<String, Value> = edit::key_envs(file.as_ref().unwrap_or(&config))
        .into_iter()
        .map(|name| {
            let set = std::env::var_os(&name).is_some();
            (name, set.into())
        })
        .collect();
    Ok(json!({
        "version": text.as_deref().map(version),
        "editable": editable,
        "config": config,
        "fields": fields,
        "pending_restart": status.pending,
        "reload_error": status.error,
        "env": env,
    }))
}

#[derive(Deserialize)]
struct Update {
    /// The file's version the change was made on.
    version: String,
    /// New values by setting; null removes one, back to its default.
    values: BTreeMap<String, Value>,
}

/// `PUT /admin/config/{section}`: change settings of one panel section in the file, if it is
/// still the version the panel read, and reload it.
async fn update(
    State(app): State<App>,
    headers: HeaderMap,
    Path(section): Path<String>,
    Json(update): Json<Update>,
) -> ApiResult<Json<Value>> {
    let auth = admin(&app, &headers).await?;
    let path = (app.running.path.clone())
        .filter(|path| edit::writable(path))
        .context(Code::ConfigReadOnly)?;
    let running = app.config();
    let known = settings(&running);
    for key in update.values.keys() {
        require!(
            known.contains_key(key)
                && rule_of(key).is_some_and(|rule| rule.panel && rule.section == section),
            Code::ConfigFieldLocked
        );
    }
    // The admin's own page must stay allowed.
    if let (Some(origins), Some(origin)) = (
        update.values.get("allowed_origins"),
        headers.get(header::ORIGIN),
    ) {
        require!(
            origins
                .as_array()
                .into_iter()
                .flatten()
                .any(|allowed| { allowed.as_str().map(str::as_bytes) == Some(origin.as_bytes()) }),
            Code::OriginRequired
        );
    }
    let values = update.values.clone();
    let reloaded = app.clone();
    tokio::task::spawn_blocking(move || {
        edit::locked(&path, || {
            let text = std::fs::read_to_string(&path)?;
            require!(version(&text) == update.version, Code::ConfigChanged);
            let allowed = edit::key_envs(&parse_toml(&text).unwrap_or((*running).clone()));
            for (key, value) in &values {
                require!(
                    edit::names_allowed_env(key, value, &allowed),
                    Code::EnvNotAllowed
                );
            }
            let next = edit::apply(&text, &values)?;
            parse_toml::<Config>(&next)
                .and_then(|config| config.validate())
                .map_err(|error| {
                    anyhow::Error::new(Detail(format!("{error:#}"))).context(Code::InvalidConfig)
                })?;
            edit::replace(&path, &next)?;
            reloaded.running.reload()
        })
    })
    .await??;
    let mut tx = app.store.pool.begin().await?;
    let paths: Vec<&String> = update.values.keys().collect();
    audit(
        &mut tx,
        &auth,
        "admin.config.update",
        &section,
        json!({"paths": paths}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(view(&app)?))
}

/// `POST /admin/search/reindex`: index everything again, after the embedding model changed.
async fn reindex(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let auth = admin(&app, &headers).await?;
    let queued = crate::search::reindex(&app.store.pool).await?;
    let mut tx = app.store.pool.begin().await?;
    audit(
        &mut tx,
        &auth,
        "admin.search.reindex",
        "",
        json!({"queued": queued}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"queued": queued})))
}
