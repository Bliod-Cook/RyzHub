//! Plugins (M5): packages of remote MCP tools, skills, commands and UI that members install.
//!
//! A source is fetched and parsed into an immutable version. Installing confirms what the
//! version asks for (servers, permissions, settings, the full text of its skills and commands)
//! and the tools its servers list; a tool whose definition changes later is withheld until the
//! member confirms it again. Each run records the plugins it starts with; the MCP gateway (Work)
//! and the Chat loop offer their tools as `<plugin>__<tool>`. Instance blocks and a member
//! turning a plugin off take effect at once, also for runs in progress.
pub mod api;
pub mod apps;
pub mod client;
pub mod elicit;
pub mod fetch;
pub mod oauth;
pub mod package;

use anyhow::{Context, ensure};
use base64::Engine as _;
use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{app::App, error::Code, store::Tx};
use package::{Manifest, Skill};

/// Where a plugin comes from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// A package archive (zip or tar.gz) at a URL.
    Url { url: String },
    /// A git repository, at a branch, tag or commit (empty: its default branch).
    Git {
        url: String,
        #[serde(default)]
        reference: String,
    },
    /// An archive the member uploaded.
    Upload {
        #[serde(default)]
        file_name: String,
    },
    /// An entry of the official catalog, by name.
    Catalog { name: String },
    /// A remote MCP server alone.
    Mcp {
        url: String,
        #[serde(default)]
        name: String,
    },
}

impl Source {
    /// The policy's name for this kind of source (`plugin_sources`).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Url { .. } => "url",
            Self::Git { .. } => "git",
            Self::Upload { .. } => "upload",
            Self::Catalog { .. } => "catalog",
            Self::Mcp { .. } => "mcp",
        }
    }
}

/// A source, fetched and read.
pub struct Obtained {
    pub manifest: Manifest,
    pub content_hash: String,
    pub commit: Option<String>,
    /// `signed`, `official` or `unverified`.
    pub trust: &'static str,
}

pub fn fetcher(app: &App) -> fetch::Fetch {
    fetch::Fetch::new(&app.config.plugins)
}

/// Fetch and read `source`; `upload` is the archive of an upload.
pub async fn obtain(app: &App, source: &Source, upload: Option<&[u8]>) -> anyhow::Result<Obtained> {
    let fetch = fetcher(app);
    let (manifest, content_hash, commit) = match source {
        Source::Url { url } => {
            let fetched = fetch
                .send(
                    reqwest::Method::GET,
                    url,
                    HeaderMap::new(),
                    None,
                    package::MAX_BYTES,
                )
                .await?;
            ensure!(fetched.status.is_success(), Code::PluginFetchFailed);
            let (manifest, hash) = parse_archive(app, &fetched.body).await?;
            (manifest, hash, None)
        }
        Source::Git { url, reference } => {
            let (archive, commit) = git_archive(app, url, reference).await?;
            let (manifest, hash) = parse_archive(app, &archive).await?;
            (manifest, hash, Some(commit))
        }
        Source::Upload { .. } => {
            let archive = upload.context(Code::InvalidInput)?;
            let (manifest, hash) = parse_archive(app, archive).await?;
            (manifest, hash, None)
        }
        Source::Mcp { url, name } => {
            let manifest = package::minimal(url, (!name.is_empty()).then_some(name.as_str()))
                .map_err(|_| Code::PluginInvalid)?;
            fetch
                .check(&url::Url::parse(url).context(Code::UnsafeDestination)?)
                .await?;
            let hash = format!(
                "{:x}",
                Sha256::digest(format!("mcp\0{url}\0{}", manifest.name))
            );
            (manifest, hash, None)
        }
        Source::Catalog { name } => {
            let entry = catalog_entry(app, name).await?;
            let inner: Source =
                serde_json::from_value(entry["source"].clone()).context(Code::PluginFetchFailed)?;
            ensure!(
                !matches!(inner, Source::Catalog { .. } | Source::Upload { .. }),
                Code::PluginFetchFailed
            );
            let mut obtained = Box::pin(obtain(app, &inner, None)).await?;
            let reviewed = entry["content_hash"]
                .as_str()
                .is_none_or(|hash| hash == obtained.content_hash);
            if reviewed && obtained.trust == "unverified" {
                obtained.trust = "official";
            }
            return Ok(obtained);
        }
    };
    let trust = if signed(app, &manifest, &content_hash) {
        "signed"
    } else {
        "unverified"
    };
    Ok(Obtained {
        manifest,
        content_hash,
        commit,
        trust,
    })
}

/// Whether the package carries a valid signature by a trusted publisher over its content.
fn signed(app: &App, manifest: &Manifest, content_hash: &str) -> bool {
    manifest.signature.as_ref().is_some_and(|(key, signature)| {
        verify(
            &app.config.plugins.trusted_keys,
            key,
            signature,
            content_hash,
        )
    })
}

/// Whether `signature` (base64) is `key`'s ed25519 signature of `content_hash`, `key` being one
/// of the `trusted` publishers (id → base64 public key).
pub fn verify(
    trusted: &std::collections::BTreeMap<String, String>,
    key: &str,
    signature: &str,
    content_hash: &str,
) -> bool {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let decode = |text: &str| base64::engine::general_purpose::STANDARD.decode(text).ok();
    let Some(public) = trusted.get(key).and_then(|k| decode(k)) else {
        return false;
    };
    let (Ok(public), Some(signature)) = (<[u8; 32]>::try_from(public), decode(signature)) else {
        return false;
    };
    let (Ok(key), Ok(signature)) = (
        VerifyingKey::from_bytes(&public),
        Signature::from_slice(&signature),
    ) else {
        return false;
    };
    key.verify(content_hash.as_bytes(), &signature).is_ok()
}

/// The official catalog: `[{name, description, source, content_hash}]`.
pub async fn catalog(app: &App) -> anyhow::Result<Vec<Value>> {
    let url = &app.config.plugins.catalog_url;
    if url.is_empty() {
        return Ok(vec![]);
    }
    let fetched = fetcher(app)
        .send(
            reqwest::Method::GET,
            url,
            HeaderMap::new(),
            None,
            2 * 1024 * 1024,
        )
        .await?;
    ensure!(fetched.status.is_success(), Code::PluginFetchFailed);
    let index: Value = serde_json::from_slice(&fetched.body).context(Code::PluginFetchFailed)?;
    Ok(index
        .as_array()
        .or_else(|| index["plugins"].as_array())
        .cloned()
        .unwrap_or_default())
}

async fn catalog_entry(app: &App, name: &str) -> anyhow::Result<Value> {
    catalog(app)
        .await?
        .into_iter()
        .find(|entry| entry["name"] == name)
        .context(Code::PluginNotFound)
}

/// Unpack and read an archive in a `parse-plugin` process of its own.
async fn parse_archive(app: &App, archive: &[u8]) -> anyhow::Result<(Manifest, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    ensure!(archive.len() <= package::MAX_BYTES, Code::PluginTooLarge);
    let program = crate::attachments::renderer(&app.config.attachment_renderer)?;
    let mut child = tokio::process::Command::new(program)
        .arg("parse-plugin")
        .arg(&app.config.plugins.namespace)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().context("parser input")?;
    let mut stdout = child.stdout.take().context("parser output")?;
    let archive = archive.to_vec();
    let exchange = async move {
        let feed = async move {
            stdin.write_all(&archive).await?;
            drop(stdin);
            Ok::<_, std::io::Error>(())
        };
        let mut reply = vec![];
        let mut limited = (&mut stdout).take(8 * 1024 * 1024);
        let read = limited.read_to_end(&mut reply);
        let (_, read) = tokio::join!(feed, read);
        read?;
        child.wait().await?;
        Ok::<_, std::io::Error>(reply)
    };
    let reply = tokio::time::timeout(std::time::Duration::from_secs(30), exchange)
        .await
        .context(Code::PluginInvalid)??;
    let reply: Value = serde_json::from_slice(&reply).context(Code::PluginInvalid)?;
    if let Some(error) = reply["error"].as_str() {
        tracing::info!(error, "plugin package refused");
        return Err(
            anyhow::Error::new(crate::error::Detail(error.to_owned())).context(Code::PluginInvalid)
        );
    }
    Ok((
        serde_json::from_value(reply["manifest"].clone())?,
        reply["content_hash"]
            .as_str()
            .context("content hash")?
            .to_owned(),
    ))
}

/// A shallow clone of `url` at `reference`, as an archive, and its commit. Git connects to
/// the address checked here.
async fn git_archive(app: &App, url: &str, reference: &str) -> anyhow::Result<(Vec<u8>, String)> {
    let parsed = url::Url::parse(url).context(Code::UnsafeDestination)?;
    let address = fetcher(app).check(&parsed).await?;
    ensure!(
        reference.is_empty()
            || reference
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
                && !reference.starts_with('-'),
        Code::InvalidInput
    );
    let host = parsed.host_str().context(Code::UnsafeDestination)?;
    let port = parsed
        .port_or_known_default()
        .context(Code::UnsafeDestination)?;
    let ip = match address.ip() {
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        ip => ip.to_string(),
    };
    let dir = tempfile::tempdir()?;
    let checkout = dir.path().join("checkout");
    let git = |args: &[&str]| {
        let mut command = tokio::process::Command::new("git");
        command
            .args(["-c", &format!("http.curloptResolve={host}:{port}:{ip}")])
            .args([
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.https.allow=always",
            ])
            .args([
                "-c",
                "protocol.http.allow=always",
                "-c",
                "http.followRedirects=false",
            ])
            .args(["-c", "core.symlinks=false", "-c", "submodule.recurse=false"])
            .args(args)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", dir.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command
    };
    let run = |mut command: tokio::process::Command| async move {
        let output = tokio::time::timeout(std::time::Duration::from_secs(90), command.output())
            .await
            .context(Code::PluginFetchFailed)??;
        if !output.status.success() {
            tracing::info!(stderr = %String::from_utf8_lossy(&output.stderr), "git failed");
            anyhow::bail!(Code::PluginFetchFailed);
        }
        Ok::<_, anyhow::Error>(output.stdout)
    };
    let checkout_path = checkout.to_string_lossy().into_owned();
    let commit_id = reference.len() == 40 && reference.bytes().all(|b| b.is_ascii_hexdigit());
    if commit_id {
        run(git(&["init", "-q", &checkout_path])).await?;
        run(git(&[
            "-C",
            &checkout_path,
            "fetch",
            "-q",
            "--depth",
            "1",
            url,
            reference,
        ]))
        .await?;
        run(git(&["-C", &checkout_path, "checkout", "-q", "FETCH_HEAD"])).await?;
    } else {
        let mut args = vec![
            "clone",
            "-q",
            "--depth",
            "1",
            "--single-branch",
            "--no-tags",
        ];
        if !reference.is_empty() {
            args.extend(["--branch", reference]);
        }
        args.extend(["--", url, &checkout_path]);
        run(git(&args)).await?;
    }
    let commit = String::from_utf8(run(git(&["-C", &checkout_path, "rev-parse", "HEAD"])).await?)?
        .trim()
        .to_owned();
    let archive = tokio::task::spawn_blocking(move || package::pack_dir(&checkout)).await??;
    Ok((archive, commit))
}

/// Whether the instance stopped this plugin, by name or content.
pub async fn blocked(tx: &mut Tx<'_>, name: &str, content_hash: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM plugin_blocks WHERE name=$1 OR content_hash=$2)",
    )
    .bind(name)
    .bind(content_hash)
    .fetch_one(&mut **tx)
    .await?)
}

/// A tool's name for models: `<plugin>__<tool>`, in the characters providers accept, at most
/// 64 long (a digest keeps long names apart).
pub fn tool_name(plugin: &str, tool: &str) -> String {
    let clean = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let name = format!("{}__{}", clean(plugin), clean(tool));
    if name.len() <= 64 {
        return name;
    }
    let digest = format!("{:x}", Sha256::digest(&name));
    format!("{}_{}", &name[..55], &digest[..8])
}

/// The digest a tool's definition is confirmed by: everything a model reads of it.
pub fn tool_digest(definition: &Value) -> String {
    let read = json!({
        "name": definition["name"],
        "title": definition["title"],
        "description": definition["description"],
        "inputSchema": definition["inputSchema"],
        "outputSchema": definition["outputSchema"],
        "annotations": definition["annotations"],
    });
    format!("{:x}", Sha256::digest(read.to_string()))
}

/// The plugins a new run of `conversation` starts with: its creator's installations that are
/// confirmed and on, unless the project or the conversation turns them off.
pub async fn snapshot(tx: &mut Tx<'_>, org: Uuid, conversation: Uuid) -> anyhow::Result<Value> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT i.id,i.version_id FROM plugin_installations i
         JOIN conversations c ON c.org_id=i.org_id AND c.id=$2
         LEFT JOIN conversation_plugins cp ON cp.conversation_id=c.id AND cp.installation_id=i.id
         LEFT JOIN project_plugins pp ON pp.project_id=c.project_id AND pp.installation_id=i.id
         WHERE i.org_id=$1 AND i.user_id=c.creator_id AND i.state<>'pending'
           AND coalesce(cp.enabled,pp.enabled,i.enabled)
         ORDER BY i.created_at",
    )
    .bind(org)
    .bind(conversation)
    .fetch_all(&mut **tx)
    .await?;
    Ok(Value::Array(
        rows.into_iter()
            .map(
                |(installation, version)| json!({"installation": installation, "version": version}),
            )
            .collect(),
    ))
}

/// Record the plugins `run` starts with.
pub async fn fix_run(
    tx: &mut Tx<'_>,
    org: Uuid,
    conversation: Uuid,
    run: Uuid,
) -> anyhow::Result<()> {
    let plugins = snapshot(tx, org, conversation).await?;
    sqlx::query("UPDATE runs SET plugins=$3 WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(run)
        .bind(plugins)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A plugin tool as a run offers it.
#[derive(Clone, Debug)]
pub struct RunTool {
    /// `<plugin>__<tool>`.
    pub name: String,
    pub plugin: String,
    pub installation: Uuid,
    pub server: String,
    /// The tool's name at its server.
    pub remote: String,
    pub definition: Value,
    /// `allow`, `ask` or `deny`: how the plugin's permissions treat it.
    pub rule: String,
    /// An MCP App page that shows its results (`ui://…`), when the plugin may use them.
    pub ui: Option<String>,
}

impl RunTool {
    /// The definition in the Chat loop's format.
    pub fn chat_definition(&self) -> Value {
        json!({
            "name": self.name,
            "description": format!(
                "[{}] {}",
                self.plugin,
                self.definition["description"].as_str().unwrap_or_default()
            ),
            "input_schema": schema(&self.definition["inputSchema"]),
        })
    }

    /// The definition as the gateway lists it to harnesses.
    pub fn mcp_definition(&self) -> Value {
        let mut definition = self.definition.clone();
        definition["name"] = json!(self.name);
        definition["inputSchema"] = schema(&self.definition["inputSchema"]);
        definition
    }
}

fn schema(input: &Value) -> Value {
    if input.is_object() {
        input.clone()
    } else {
        json!({"type": "object", "properties": {}})
    }
}

/// The plugin tools `run` may call now: its plugins that are still on and not blocked, and
/// their confirmed tools.
pub async fn run_tools(
    app: &App,
    tx: &mut Tx<'_>,
    org: Uuid,
    run: Uuid,
) -> anyhow::Result<Vec<RunTool>> {
    if !app.config.plugins.enabled {
        return Ok(vec![]);
    }
    type Row = (Uuid, String, Value, String, String, Value);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT i.id,p.name,v.manifest,t.server,t.tool,t.definition
         FROM runs r CROSS JOIN LATERAL jsonb_array_elements(r.plugins) e
         JOIN plugin_installations i ON i.org_id=r.org_id AND i.id=(e->>'installation')::uuid
         JOIN plugins p ON p.org_id=i.org_id AND p.id=i.plugin_id
         JOIN plugin_versions v ON v.org_id=i.org_id AND v.id=i.version_id
         JOIN plugin_tools t ON t.installation_id=i.id AND t.approved_digest=t.digest
         WHERE r.org_id=$1 AND r.id=$2 AND i.enabled AND i.state<>'pending'
           AND NOT EXISTS(SELECT 1 FROM plugin_blocks b WHERE b.name=p.name OR b.content_hash=v.content_hash)
         ORDER BY p.name,t.server,t.tool",
    )
    .bind(org)
    .bind(run)
    .fetch_all(&mut **tx)
    .await?;
    let policy = crate::store::effective_policy(tx, &app.config.policy, org).await?;
    let apps = policy.allow_mcp_apps != Some(false);
    let mut tools = vec![];
    for (installation, plugin, manifest, server, remote, definition) in rows {
        let manifest: Manifest = serde_json::from_value(manifest)?;
        let rules = &manifest.permissions.tools;
        let read_only = definition["annotations"]["readOnlyHint"] == true;
        let rule = rules.tools.get(&remote).cloned().unwrap_or_else(|| {
            if read_only {
                rules.read_only.clone()
            } else {
                rules.default.clone()
            }
        });
        if rule == "deny" {
            continue;
        }
        let ui = definition["_meta"]["ui"]["resourceUri"]
            .as_str()
            .or_else(|| definition["_meta"]["ui/resourceUri"].as_str())
            .filter(|uri| apps && manifest.ui.mcp_apps && uri.starts_with("ui://"))
            .map(str::to_owned);
        tools.push(RunTool {
            name: tool_name(&plugin, &remote),
            plugin,
            installation,
            server,
            remote,
            definition,
            rule,
            ui,
        });
    }
    Ok(tools)
}

/// The skills of `run`'s plugins, with each plugin's name.
pub async fn run_skills(
    tx: &mut Tx<'_>,
    org: Uuid,
    run: Uuid,
) -> anyhow::Result<Vec<(String, Skill)>> {
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT p.name,v.manifest FROM runs r CROSS JOIN LATERAL jsonb_array_elements(r.plugins) e
         JOIN plugin_installations i ON i.org_id=r.org_id AND i.id=(e->>'installation')::uuid
         JOIN plugins p ON p.org_id=i.org_id AND p.id=i.plugin_id
         JOIN plugin_versions v ON v.org_id=i.org_id AND v.id=i.version_id
         WHERE r.org_id=$1 AND r.id=$2 AND i.enabled AND i.state<>'pending'
           AND NOT EXISTS(SELECT 1 FROM plugin_blocks b WHERE b.name=p.name OR b.content_hash=v.content_hash)
         ORDER BY p.name",
    )
    .bind(org)
    .bind(run)
    .fetch_all(&mut **tx)
    .await?;
    let mut skills = vec![];
    for (plugin, manifest) in rows {
        let manifest: Manifest = serde_json::from_value(manifest)?;
        skills.extend(
            manifest
                .skills
                .into_iter()
                .map(|skill| (plugin.clone(), skill)),
        );
    }
    Ok(skills)
}

/// The commands the plugins on in `conversation` offer: `[{installation, plugin, name, title,
/// description, params}]`.
pub async fn commands(
    tx: &mut Tx<'_>,
    org: Uuid,
    conversation: Uuid,
) -> anyhow::Result<Vec<Value>> {
    let on = snapshot(tx, org, conversation).await?;
    let ids: Vec<Uuid> = on
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["installation"].as_str()?.parse().ok())
        .collect();
    let rows: Vec<(Uuid, String, Value)> = sqlx::query_as(
        "SELECT i.id,p.name,v.manifest FROM plugin_installations i
         JOIN plugins p ON p.org_id=i.org_id AND p.id=i.plugin_id
         JOIN plugin_versions v ON v.org_id=i.org_id AND v.id=i.version_id
         WHERE i.org_id=$1 AND i.id=ANY($2)
           AND NOT EXISTS(SELECT 1 FROM plugin_blocks b WHERE b.name=p.name OR b.content_hash=v.content_hash)
         ORDER BY p.name",
    )
    .bind(org)
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?;
    let mut commands = vec![];
    for (installation, plugin, manifest) in rows {
        let manifest: Manifest = serde_json::from_value(manifest)?;
        for command in manifest.commands {
            commands.push(json!({
                "installation": installation,
                "plugin": plugin,
                "name": command.name,
                "title": command.title,
                "description": command.description,
                "params": command.params,
            }));
        }
    }
    Ok(commands)
}

/// The text a plugin command becomes, for the member who has it on in `conversation`.
pub async fn command_prompt(
    tx: &mut Tx<'_>,
    org: Uuid,
    conversation: Uuid,
    installation: Uuid,
    name: &str,
    params: &serde_json::Map<String, Value>,
) -> anyhow::Result<String> {
    let on = snapshot(tx, org, conversation).await?;
    ensure!(
        on.as_array()
            .into_iter()
            .flatten()
            .any(|e| e["installation"] == json!(installation)),
        Code::PluginNotFound
    );
    let (plugin, manifest): (String, Value) = sqlx::query_as(
        "SELECT p.name,v.manifest FROM plugin_installations i
         JOIN plugins p ON p.org_id=i.org_id AND p.id=i.plugin_id
         JOIN plugin_versions v ON v.org_id=i.org_id AND v.id=i.version_id
         WHERE i.org_id=$1 AND i.id=$2",
    )
    .bind(org)
    .bind(installation)
    .fetch_one(&mut **tx)
    .await?;
    let manifest: Manifest = serde_json::from_value(manifest)?;
    let command = manifest
        .commands
        .iter()
        .find(|c| c.name == name)
        .context(Code::PluginNotFound)?;
    let prompt = package::render(command, params).map_err(|_| Code::InvalidArguments)?;
    Ok(format!("/{plugin}:{name}\n\n{prompt}"))
}

/// The headers a plugin server's requests carry: the package's fixed ones, settings sent as
/// headers, and the member's connected account.
async fn server_headers(
    app: &App,
    org: Uuid,
    user: Uuid,
    installation: Uuid,
    server: &package::Server,
) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in &server.headers {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).context(Code::PluginInvalid)?,
            HeaderValue::from_str(value).context(Code::PluginInvalid)?,
        );
    }
    let mut tx = app.store.tx(org).await?;
    let (manifest, config, secrets): (Value, Value, Value) = sqlx::query_as(
        "SELECT v.manifest,i.config,i.secrets FROM plugin_installations i
         JOIN plugin_versions v ON v.org_id=i.org_id AND v.id=i.version_id
         WHERE i.org_id=$1 AND i.id=$2",
    )
    .bind(org)
    .bind(installation)
    .fetch_one(&mut *tx)
    .await?;
    let manifest: Manifest = serde_json::from_value(manifest)?;
    for (field, spec) in manifest
        .user_config
        .iter()
        .filter(|(_, s)| !s.header.is_empty())
    {
        let value = if spec.secret {
            match secrets[field]
                .as_str()
                .and_then(|id| id.parse::<Uuid>().ok())
            {
                Some(id) => {
                    let purpose = format!("plugin:{installation}:{field}");
                    String::from_utf8(
                        crate::vault::get(&mut tx, &app.config.llm, org, id, &purpose).await?,
                    )?
                }
                None => continue,
            }
        } else {
            match &config[field] {
                Value::Null => continue,
                Value::String(text) => text.clone(),
                other => other.to_string(),
            }
        };
        headers.insert(
            HeaderName::from_bytes(spec.header.as_bytes()).context(Code::PluginInvalid)?,
            HeaderValue::from_str(&format!("{}{value}", spec.prefix))
                .context(Code::InvalidArguments)?,
        );
    }
    tx.commit().await?;
    if let Some(token) = oauth::token(app, org, user, installation, &server.name).await? {
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).context(Code::PluginAuthRequired)?,
        );
    }
    Ok(headers)
}

/// An installation's version, plugin name and owner.
async fn installation(
    tx: &mut Tx<'_>,
    org: Uuid,
    id: Uuid,
) -> anyhow::Result<(String, Manifest, Uuid, String)> {
    let (name, manifest, user, state): (String, Value, Uuid, String) = sqlx::query_as(
        "SELECT p.name,v.manifest,i.user_id,i.state FROM plugin_installations i
         JOIN plugins p ON p.org_id=i.org_id AND p.id=i.plugin_id
         JOIN plugin_versions v ON v.org_id=i.org_id AND v.id=i.version_id
         WHERE i.org_id=$1 AND i.id=$2",
    )
    .bind(org)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .context(Code::PluginNotFound)?;
    Ok((name, serde_json::from_value(manifest)?, user, state))
}

/// Open a session with a server of an installation, for its member.
pub async fn session(
    app: &App,
    org: Uuid,
    installation: Uuid,
    server: &str,
) -> anyhow::Result<client::Session> {
    let mut tx = app.store.tx(org).await?;
    let (_, manifest, user, _) = self::installation(&mut tx, org, installation).await?;
    tx.commit().await?;
    let server = manifest
        .servers
        .iter()
        .find(|s| s.name == server && s.enabled)
        .context(Code::PluginNotFound)?;
    let headers = server_headers(app, org, user, installation, server).await?;
    client::Session::open(&fetcher(app), &server.url, headers).await
}

/// Call a run's plugin tool. `answers` respond to an earlier `input_required`.
pub async fn call(
    app: &App,
    org: Uuid,
    tool: &RunTool,
    arguments: Value,
    answers: Option<(Value, Option<String>)>,
) -> anyhow::Result<client::Reply> {
    let session = session(app, org, tool.installation, &tool.server).await?;
    session.call(&tool.remote, arguments, answers).await
}

/// What listing an installation's tools found.
#[derive(Debug, Default, Serialize)]
pub struct Listed {
    pub tools: usize,
    /// Tools withheld until the member confirms their new definitions.
    pub changed: Vec<String>,
    /// Servers that need the member to connect an account.
    pub needs_connection: Vec<String>,
    /// Servers that could not be reached.
    pub failed: Vec<String>,
}

/// List an installation's tools from its servers and record them. With `approve`, the member
/// has just confirmed them; otherwise a tool that is new or changed is withheld and the
/// installation waits for confirmation.
pub async fn refresh_tools(
    app: &App,
    org: Uuid,
    installation: Uuid,
    approve: bool,
) -> anyhow::Result<Listed> {
    let mut tx = app.store.tx(org).await?;
    let (_, manifest, _, _) = self::installation(&mut tx, org, installation).await?;
    tx.commit().await?;
    let mut listed = Listed::default();
    for server in manifest.servers.iter().filter(|s| s.enabled) {
        let tools = match session(app, org, installation, &server.name).await {
            Ok(session) => session.tools().await,
            Err(error) => Err(error),
        };
        let tools = match tools {
            Ok(tools) => tools,
            Err(error) if error.downcast_ref::<client::AuthRequired>().is_some() => {
                listed.needs_connection.push(server.name.clone());
                continue;
            }
            Err(error) => {
                tracing::info!(
                    server = server.name,
                    error = format!("{error:#}"),
                    "plugin tools not listed"
                );
                listed.failed.push(server.name.clone());
                continue;
            }
        };
        let mut tx = app.store.tx(org).await?;
        let mut names = vec![];
        for definition in tools.into_iter().take(500) {
            let Some(tool) = definition["name"].as_str().map(str::to_owned) else {
                continue;
            };
            let digest = tool_digest(&definition);
            let approved: Option<String> = sqlx::query_scalar(
                "INSERT INTO plugin_tools(org_id,installation_id,server,tool,definition,digest,approved_digest)
                 VALUES($1,$2,$3,$4,$5,$6,CASE WHEN $7 THEN $6 END)
                 ON CONFLICT(installation_id,server,tool) DO UPDATE SET definition=$5,digest=$6,seen_at=now(),
                   approved_digest=CASE WHEN $7 THEN $6 ELSE plugin_tools.approved_digest END
                 RETURNING approved_digest",
            )
            .bind(org)
            .bind(installation)
            .bind(&server.name)
            .bind(&tool)
            .bind(&definition)
            .bind(&digest)
            .bind(approve)
            .fetch_one(&mut *tx)
            .await?;
            if approved.as_deref() != Some(digest.as_str()) {
                listed.changed.push(tool.clone());
            }
            names.push(tool);
            listed.tools += 1;
        }
        sqlx::query(
            "DELETE FROM plugin_tools WHERE org_id=$1 AND installation_id=$2 AND server=$3 AND NOT tool=ANY($4)",
        )
        .bind(org)
        .bind(installation)
        .bind(&server.name)
        .bind(&names)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    if !listed.changed.is_empty() {
        let mut tx = app.store.tx(org).await?;
        sqlx::query(
            "UPDATE plugin_installations SET state='needs_reconfirm',updated_at=now()
             WHERE org_id=$1 AND id=$2 AND state='active'",
        )
        .bind(org)
        .bind(installation)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok(listed)
}

/// The base64 of `bytes`, for uploads carried in JSON.
pub fn decode_upload(text: &str) -> anyhow::Result<Bytes> {
    Ok(base64::engine::general_purpose::STANDARD
        .decode(text)
        .context(Code::InvalidInput)?
        .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_by_trusted_publishers_verify() {
        use ed25519_dalek::{Signer, SigningKey};
        let signing = SigningKey::from_bytes(&[7; 32]);
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let trusted = [(
            "acme".to_owned(),
            encode(signing.verifying_key().as_bytes()),
        )]
        .into();
        let signature = encode(&signing.sign(b"abc123").to_bytes());
        assert!(verify(&trusted, "acme", &signature, "abc123"));
        assert!(
            !verify(&trusted, "acme", &signature, "abc124"),
            "other content"
        );
        assert!(
            !verify(&trusted, "other", &signature, "abc123"),
            "unknown publisher"
        );
        assert!(!verify(&trusted, "acme", "not base64!", "abc123"));
    }

    #[test]
    fn tool_names_fit_what_providers_accept() {
        assert_eq!(tool_name("acme-tracker", "lookup"), "acme-tracker__lookup");
        assert_eq!(tool_name("mcp.example.com", "a b"), "mcp_example_com__a_b");
        let long = tool_name(&"p".repeat(60), &"t".repeat(60));
        assert_eq!(long.len(), 64);
        assert_ne!(long, tool_name(&"p".repeat(60), &"u".repeat(60)));
    }
}
