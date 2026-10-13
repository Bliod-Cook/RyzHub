//! Plugin packages. A package is Agent Plugins 1.0.0 (`plugin.json`, `mcp.json`, `skills/`)
//! with the platform's extension namespace for commands, permissions, UI and settings, or a
//! Claude Code plugin (`.claude-plugin/plugin.json`), which is converted. A remote MCP address
//! alone is a minimal plugin.
//!
//! Unpacking and parsing run in a process of their own (`ryzhub-server parse-plugin`) with
//! memory and CPU limits: packages come from anywhere.
use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
};

pub const PLUGIN_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
pub const MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";
/// Most files, bytes and bytes per file a package may hold, unpacked.
pub const MAX_FILES: usize = 1000;
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_FILE: usize = 2 * 1024 * 1024;
/// Memory and CPU the parsing process may use.
const CHILD_MEMORY: u64 = 768 * 1024 * 1024;
const CHILD_CPU_SECS: u64 = 20;

/// A package's files by relative path.
pub type Files = BTreeMap<String, Vec<u8>>;

/// What a package provides and asks for, as the platform keeps it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: Value,
    pub homepage: String,
    /// `agent-plugins`, `claude-code` or `mcp` (a remote MCP address alone).
    pub format: String,
    pub servers: Vec<Server>,
    pub skills: Vec<Skill>,
    pub commands: Vec<Command>,
    pub permissions: Permissions,
    pub ui: Ui,
    pub user_config: BTreeMap<String, ConfigField>,
    /// Parts the platform does not run, said at install.
    pub warnings: Vec<String>,
    /// The publisher's signature over the content hash: key id and base64 signature.
    pub signature: Option<(String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    pub name: String,
    /// `streamable-http` runs; `stdio` (MCP in the sandbox) is not supported yet.
    pub kind: String,
    pub url: String,
    /// Fixed request headers from the package (public data).
    pub headers: BTreeMap<String, String>,
    pub enabled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// `SKILL.md`, whole.
    pub body: String,
    /// The skill's other text files, by path under its directory.
    pub files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Command {
    pub name: String,
    pub title: String,
    pub description: String,
    /// The prompt, with `{{param}}` for each parameter.
    pub prompt: String,
    /// JSON schema of each parameter.
    pub params: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Permissions {
    /// How tool calls are treated: `allow`, `ask` or `deny` by default, for tools annotated
    /// read-only, and per tool.
    pub tools: ToolRules,
    /// Workspace access a remote plugin asks for; `none` in v1.
    pub workspace: String,
    /// Hosts a plugin running in the sandbox may reach (v2).
    pub network: Vec<String>,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            tools: ToolRules::default(),
            workspace: "none".into(),
            network: vec![],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ToolRules {
    pub default: String,
    pub read_only: String,
    pub tools: BTreeMap<String, String>,
}

impl Default for ToolRules {
    fn default() -> Self {
        Self {
            default: "ask".into(),
            read_only: "allow".into(),
            tools: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Ui {
    pub declarative: bool,
    pub mcp_apps: bool,
}

/// A setting the member gives the plugin.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigField {
    /// `string`, `number` or `boolean`.
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub description: String,
    pub required: bool,
    /// Kept in the vault and sent only to the plugin's servers.
    pub secret: bool,
    pub default: Value,
    /// The request header the value goes in, with `prefix` before it (e.g. `Bearer `).
    pub header: String,
    pub prefix: String,
}

impl Manifest {
    /// Everything that is a prompt or reaches a server: what the member confirms.
    pub fn permissions_digest(&self) -> String {
        let asked = json!({
            "servers": self.servers.iter().map(|s| json!({"name":s.name,"url":s.url,"headers":s.headers})).collect::<Vec<_>>(),
            "permissions": self.permissions,
            "ui": self.ui,
            "user_config": self.user_config,
        });
        format!("{:x}", Sha256::digest(asked.to_string()))
    }

    /// Whether `other` asks for more than this: servers, tool rules, UI or settings it did not.
    pub fn widened_by(&self, other: &Manifest) -> bool {
        let looser = |was: &str, now: &str| rank(now) > rank(was);
        let tools = &other.permissions.tools;
        other.servers.iter().any(|s| {
            !self
                .servers
                .iter()
                .any(|t| t.url == s.url && t.headers == s.headers)
        }) || looser(&self.permissions.tools.default, &tools.default)
            || looser(&self.permissions.tools.read_only, &tools.read_only)
            || tools.tools.iter().any(|(name, rule)| {
                let was = self
                    .permissions
                    .tools
                    .tools
                    .get(name)
                    .unwrap_or(&self.permissions.tools.default);
                looser(was, rule)
            })
            || (other.ui.mcp_apps && !self.ui.mcp_apps)
            || (other.ui.declarative && !self.ui.declarative)
            || other.permissions.workspace != self.permissions.workspace
            || other
                .user_config
                .iter()
                .any(|(name, field)| self.user_config.get(name) != Some(field))
    }
}

/// How permissive a tool rule is.
fn rank(rule: &str) -> u8 {
    match rule {
        "deny" => 0,
        "ask" => 1,
        _ => 2,
    }
}

/// The digest of a package's content: every file but its signature, by path.
pub fn content_hash(files: &Files, namespace: &str) -> String {
    let signature = format!("{namespace}/signature.json");
    let mut hasher = Sha256::new();
    for (path, bytes) in files.iter().filter(|(path, _)| **path != signature) {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    format!("{:x}", hasher.finalize())
}

/// Unpack a zip, gzipped tar or tar archive. A single top-level directory, as hosts add to
/// repository archives, is removed.
pub fn unpack(archive: &[u8]) -> anyhow::Result<Files> {
    let mut files = if archive.starts_with(b"PK\x03\x04") {
        unzip(archive)?
    } else if archive.starts_with(&[0x1f, 0x8b]) {
        untar(flate2::read::GzDecoder::new(archive))?
    } else {
        untar(archive)?
    };
    let tops: std::collections::BTreeSet<&str> = files
        .keys()
        .map(|path| path.split('/').next().unwrap_or_default())
        .collect();
    if tops.len() == 1 && files.keys().all(|path| path.contains('/')) {
        let top = format!("{}/", tops.into_iter().next().unwrap_or_default());
        files = files
            .into_iter()
            .map(|(path, bytes)| (path[top.len()..].to_owned(), bytes))
            .collect();
    }
    Ok(files)
}

/// A relative path inside the package; anything that could leave it is refused.
fn clean(path: &str) -> anyhow::Result<Option<String>> {
    let path = path.trim_start_matches("./");
    if path.is_empty() || path.ends_with('/') {
        return Ok(None);
    }
    ensure!(
        !path.starts_with('/')
            && !path.contains('\\')
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
            && path.len() <= 512,
        "unsafe path in package: {path:?}"
    );
    Ok(Some(path.to_owned()))
}

struct Budget {
    files: usize,
    bytes: usize,
}

impl Budget {
    fn take(&mut self, reader: impl Read, files: &mut Files, path: String) -> anyhow::Result<()> {
        self.files += 1;
        ensure!(
            self.files <= MAX_FILES,
            "package has more than {MAX_FILES} files"
        );
        let mut bytes = vec![];
        reader.take(MAX_FILE as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_FILE,
            "{path} is larger than {MAX_FILE} bytes"
        );
        self.bytes += bytes.len();
        ensure!(
            self.bytes <= MAX_BYTES,
            "package is larger than {MAX_BYTES} bytes unpacked"
        );
        files.insert(path, bytes);
        Ok(())
    }
}

fn unzip(archive: &[u8]) -> anyhow::Result<Files> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))?;
    let mut files = Files::new();
    let mut budget = Budget { files: 0, bytes: 0 };
    for index in 0..zip.len() {
        let entry = zip.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        ensure!(!entry.is_symlink(), "links are not allowed in packages");
        let name = entry.name()?.into_owned();
        let Some(path) = clean(&name)? else {
            continue;
        };
        budget.take(entry, &mut files, path)?;
    }
    Ok(files)
}

fn untar(reader: impl Read) -> anyhow::Result<Files> {
    let mut archive = tar::Archive::new(reader);
    let mut files = Files::new();
    let mut budget = Budget { files: 0, bytes: 0 };
    for entry in archive.entries()? {
        let entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_dir() || kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        ensure!(kind.is_file(), "only regular files are allowed in packages");
        let path = entry.path()?.to_string_lossy().into_owned();
        let Some(path) = clean(&path)? else {
            continue;
        };
        budget.take(entry, &mut files, path)?;
    }
    Ok(files)
}

/// A checkout's files, as an uncompressed tar for the parsing process. `.git` stays out.
pub fn pack_dir(root: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(vec![]);
    let mut budget = Budget { files: 0, bytes: 0 };
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            let relative = path.strip_prefix(root)?.to_string_lossy().into_owned();
            if relative == ".git" {
                continue;
            }
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let mut read = Files::new();
                budget.take(std::fs::File::open(&path)?, &mut read, relative.clone())?;
                let bytes = read.remove(&relative).unwrap_or_default();
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, &relative, bytes.as_slice())?;
            }
        }
    }
    Ok(builder.into_inner()?)
}

fn text(files: &Files, path: &str) -> anyhow::Result<Option<String>> {
    files
        .get(path)
        .map(|bytes| {
            String::from_utf8(bytes.clone()).with_context(|| format!("{path} is not UTF-8"))
        })
        .transpose()
}

fn json_file(files: &Files, path: &str) -> anyhow::Result<Option<Value>> {
    text(files, path)?
        .map(|text| {
            serde_json::from_str(&text).with_context(|| format!("{path} is not valid JSON"))
        })
        .transpose()
}

/// `---`-delimited `key: value` front matter, and the text after it.
pub fn front_matter(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut fields = BTreeMap::new();
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (fields, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (fields, text);
    };
    for line in rest[..end].lines() {
        if let Some((key, value)) = line.split_once(':')
            && !key.starts_with(' ')
        {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            fields.insert(key.trim().to_owned(), value.to_owned());
        }
    }
    let body = &rest[end + 4..];
    (fields, body.strip_prefix('\n').unwrap_or(body))
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && !name.starts_with(['-', '.'])
        && !name.ends_with(['-', '.'])
}

/// Read a package.
pub fn parse(files: &Files, namespace: &str) -> anyhow::Result<Manifest> {
    if files.contains_key("plugin.json") {
        agent_plugin(files, namespace)
    } else if files.contains_key(".claude-plugin/plugin.json") {
        claude_plugin(files)
    } else {
        bail!("no plugin.json or .claude-plugin/plugin.json in the package")
    }
}

const PLUGIN_KEYS: [&str; 10] = [
    "$schema",
    "name",
    "version",
    "description",
    "author",
    "homepage",
    "repository",
    "license",
    "keywords",
    "extensions",
];

fn agent_plugin(files: &Files, namespace: &str) -> anyhow::Result<Manifest> {
    let plugin = json_file(files, "plugin.json")?.context("plugin.json")?;
    let object = plugin.as_object().context("plugin.json is not an object")?;
    for key in object.keys() {
        ensure!(
            PLUGIN_KEYS.contains(&key.as_str()),
            "plugin.json: unknown field {key:?}"
        );
    }
    ensure!(
        plugin["$schema"] == PLUGIN_SCHEMA,
        "plugin.json: $schema must be {PLUGIN_SCHEMA}"
    );
    let name = plugin["name"].as_str().unwrap_or_default();
    ensure!(valid_name(name), "plugin.json: invalid name {name:?}");
    let mut manifest = Manifest {
        name: name.into(),
        version: plugin["version"].as_str().unwrap_or_default().into(),
        description: plugin["description"].as_str().unwrap_or_default().into(),
        author: plugin["author"].clone(),
        homepage: plugin["homepage"].as_str().unwrap_or_default().into(),
        format: "agent-plugins".into(),
        ..Default::default()
    };
    if let Some(mcp) = json_file(files, "mcp.json")? {
        ensure!(
            mcp["$schema"] == MCP_SCHEMA,
            "mcp.json: $schema must be {MCP_SCHEMA}"
        );
        for key in mcp.as_object().context("mcp.json is not an object")?.keys() {
            ensure!(
                matches!(key.as_str(), "$schema" | "mcpServers"),
                "mcp.json: unknown field {key:?}"
            );
        }
        for (server, config) in mcp["mcpServers"].as_object().into_iter().flatten() {
            manifest
                .servers
                .push(agent_server(server, config, &mut manifest.warnings)?);
        }
    }
    manifest.skills = skills(files, "skills/")?;
    // Other vendors' extensions are ignored, as the standard requires.
    if let Some(extension) = plugin["extensions"].get(namespace) {
        extend(&mut manifest, extension, files, namespace)?;
    }
    manifest.signature = signature(files, namespace)?;
    Ok(manifest)
}

fn agent_server(name: &str, config: &Value, warnings: &mut Vec<String>) -> anyhow::Result<Server> {
    ensure!(
        !name.is_empty() && name.len() <= 64,
        "mcp.json: invalid server name {name:?}"
    );
    let kind = config["type"].as_str().unwrap_or_default();
    let allowed: &[&str] = match kind {
        "stdio" => &["type", "command", "args", "env", "cwd"],
        "streamable-http" => &["type", "url", "headers"],
        other => bail!("mcp.json: server {name} has unknown type {other:?}"),
    };
    for key in config.as_object().context("server config")?.keys() {
        ensure!(
            allowed.contains(&key.as_str()),
            "mcp.json: server {name} has unknown field {key:?}"
        );
    }
    http_server(name, kind, config, warnings)
}

fn http_server(
    name: &str,
    kind: &str,
    config: &Value,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Server> {
    if kind != "streamable-http" {
        warnings.push(format!(
            "服务 {name} 需要在本地运行（{kind}），暂不支持，安装后不启用"
        ));
        return Ok(Server {
            name: name.into(),
            kind: kind.into(),
            ..Default::default()
        });
    }
    let url = config["url"].as_str().unwrap_or_default();
    let parsed = url::Url::parse(url).with_context(|| format!("server {name}: invalid url"))?;
    ensure!(
        matches!(parsed.scheme(), "https" | "http")
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none(),
        "server {name}: invalid url"
    );
    let mut headers = BTreeMap::new();
    for (header, value) in config["headers"].as_object().into_iter().flatten() {
        let value = value.as_str().context("header values are strings")?;
        ensure!(
            !header.eq_ignore_ascii_case("authorization") || !value.is_empty(),
            "server {name}: empty authorization header"
        );
        headers.insert(header.clone(), value.to_owned());
    }
    Ok(Server {
        name: name.into(),
        kind: kind.into(),
        url: url.into(),
        headers,
        enabled: true,
    })
}

/// The skills under `root`: each directory with a `SKILL.md`.
fn skills(files: &Files, root: &str) -> anyhow::Result<Vec<Skill>> {
    let mut found: BTreeMap<String, Skill> = BTreeMap::new();
    for path in files.keys().filter(|p| p.starts_with(root)) {
        let rest = &path[root.len()..];
        let Some((dir, file)) = rest.split_once('/') else {
            continue;
        };
        if file != "SKILL.md" {
            continue;
        }
        let body = text(files, path)?.unwrap_or_default();
        let (fields, _) = front_matter(&body);
        let name = fields
            .get("name")
            .cloned()
            .unwrap_or_else(|| dir.to_owned());
        let description = fields.get("description").cloned().unwrap_or_default();
        ensure!(
            !description.is_empty() && description.len() <= 1024,
            "skill {dir}: SKILL.md needs a description of at most 1024 characters"
        );
        ensure!(valid_name(&name), "skill {dir}: invalid name {name:?}");
        let prefix = format!("{root}{dir}/");
        let mut extra = BTreeMap::new();
        for (other, bytes) in files.range(prefix.clone()..) {
            if !other.starts_with(&prefix) {
                break;
            }
            if other != path
                && let Ok(text) = std::str::from_utf8(bytes)
            {
                extra.insert(other[prefix.len()..].to_owned(), text.to_owned());
            }
        }
        found.insert(
            name.clone(),
            Skill {
                name,
                description,
                body,
                files: extra,
            },
        );
    }
    Ok(found.into_values().collect())
}

/// The platform's extension: commands, permissions, UI and settings.
fn extend(
    manifest: &mut Manifest,
    extension: &Value,
    files: &Files,
    namespace: &str,
) -> anyhow::Result<()> {
    if let Some(permissions) = extension.get("permissions") {
        manifest.permissions =
            serde_json::from_value(permissions.clone()).context("invalid permissions")?;
        for rule in [
            &manifest.permissions.tools.default,
            &manifest.permissions.tools.read_only,
        ]
        .into_iter()
        .chain(manifest.permissions.tools.tools.values())
        {
            ensure!(
                matches!(rule.as_str(), "allow" | "ask" | "deny"),
                "invalid tool rule {rule:?}"
            );
        }
    }
    if let Some(ui) = extension.get("ui") {
        manifest.ui = serde_json::from_value(ui.clone()).context("invalid ui")?;
    }
    if let Some(config) = extension.get("userConfig") {
        manifest.user_config =
            serde_json::from_value(config.clone()).context("invalid userConfig")?;
        for (name, field) in &manifest.user_config {
            ensure!(
                valid_name(&name.to_ascii_lowercase().replace('_', "-"))
                    && matches!(field.kind.as_str(), "string" | "number" | "boolean"),
                "invalid setting {name:?}"
            );
            ensure!(
                field.header.is_empty()
                    || field
                        .header
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "setting {name}: invalid header"
            );
        }
    }
    for command in extension["commands"].as_array().into_iter().flatten() {
        let name = command["name"].as_str().unwrap_or_default();
        ensure!(valid_name(name), "invalid command name {name:?}");
        let path = command["prompt"].as_str().unwrap_or_default();
        ensure!(
            path.starts_with(&format!("{namespace}/")),
            "command {name}: its prompt must be a file under {namespace}/"
        );
        let prompt =
            text(files, path)?.with_context(|| format!("command {name}: {path} missing"))?;
        let params: BTreeMap<String, Value> =
            serde_json::from_value(command.get("params").cloned().unwrap_or(json!({})))
                .context("invalid command params")?;
        manifest.commands.push(Command {
            name: name.into(),
            title: command["title"].as_str().unwrap_or(name).into(),
            description: command["description"].as_str().unwrap_or_default().into(),
            prompt,
            params,
        });
    }
    Ok(())
}

fn signature(files: &Files, namespace: &str) -> anyhow::Result<Option<(String, String)>> {
    let Some(signature) = json_file(files, &format!("{namespace}/signature.json"))? else {
        return Ok(None);
    };
    Ok(Some((
        signature["key"].as_str().context("signature key")?.into(),
        signature["signature"].as_str().context("signature")?.into(),
    )))
}

/// A Claude Code plugin: skills and MCP servers as the standard's, commands as the platform's.
fn claude_plugin(files: &Files) -> anyhow::Result<Manifest> {
    let plugin = json_file(files, ".claude-plugin/plugin.json")?.context("plugin.json")?;
    let name = plugin["name"].as_str().unwrap_or_default();
    ensure!(
        valid_name(&name.to_ascii_lowercase()),
        "invalid plugin name {name:?}"
    );
    let mut manifest = Manifest {
        name: name.to_ascii_lowercase(),
        version: plugin["version"].as_str().unwrap_or_default().into(),
        description: plugin["description"].as_str().unwrap_or_default().into(),
        author: plugin["author"].clone(),
        homepage: plugin["homepage"].as_str().unwrap_or_default().into(),
        format: "claude-code".into(),
        ..Default::default()
    };
    // Servers: `.mcp.json`, or inline in plugin.json.
    let servers = match json_file(files, ".mcp.json")? {
        Some(mcp) => mcp["mcpServers"].clone(),
        None => plugin["mcpServers"].clone(),
    };
    for (server, config) in servers.as_object().into_iter().flatten() {
        let kind = match config["type"].as_str() {
            Some("http") => "streamable-http",
            Some("sse") => "sse",
            Some(other) => other,
            None if config.get("url").is_some() => "streamable-http",
            None => "stdio",
        };
        manifest
            .servers
            .push(http_server(server, kind, config, &mut manifest.warnings)?);
    }
    manifest.skills = skills(files, "skills/")?;
    for (path, _) in files.range("commands/".to_owned()..) {
        if !path.starts_with("commands/") {
            break;
        }
        let Some(stem) = path
            .strip_prefix("commands/")
            .and_then(|p| p.strip_suffix(".md"))
            .filter(|p| !p.contains('/'))
        else {
            continue;
        };
        let body = text(files, path)?.unwrap_or_default();
        let (fields, prompt) = front_matter(&body);
        let name = stem.to_ascii_lowercase();
        ensure!(valid_name(&name), "invalid command name {stem:?}");
        let mut params = BTreeMap::new();
        if prompt.contains("$ARGUMENTS") {
            params.insert(
                "arguments".to_owned(),
                json!({"type":"string","description":fields.get("argument-hint").cloned().unwrap_or_default()}),
            );
        }
        manifest.commands.push(Command {
            title: name.clone(),
            name,
            description: fields.get("description").cloned().unwrap_or_default(),
            prompt: prompt.replace("$ARGUMENTS", "{{arguments}}"),
            params,
        });
    }
    for (part, what) in [("agents/", "子 agent"), ("hooks/", "hooks")] {
        if files.keys().any(|p| p.starts_with(part)) {
            manifest
                .warnings
                .push(format!("插件带有{what}，平台暂不支持，已忽略"));
        }
    }
    Ok(manifest)
}

/// A remote MCP address alone, as a plugin of one server.
pub fn minimal(url: &str, name: Option<&str>) -> anyhow::Result<Manifest> {
    let parsed = url::Url::parse(url).context("invalid url")?;
    let host = parsed.host_str().context("invalid url")?;
    let derived: String = host
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = name.map_or(derived, str::to_owned);
    ensure!(valid_name(&name), "invalid plugin name {name:?}");
    let mut warnings = vec![];
    let server = http_server(
        &name,
        "streamable-http",
        &json!({ "url": url }),
        &mut warnings,
    )?;
    Ok(Manifest {
        name,
        format: "mcp".into(),
        servers: vec![server],
        ..Default::default()
    })
}

/// Fill a command's prompt template with its arguments, checked against its parameters.
pub fn render(command: &Command, args: &Map<String, Value>) -> anyhow::Result<String> {
    for name in args.keys() {
        ensure!(
            command.params.contains_key(name),
            "unknown parameter {name}"
        );
    }
    let mut prompt = command.prompt.clone();
    for (name, schema) in &command.params {
        let value = match args.get(name) {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Null) | None => match &schema["default"] {
                Value::Null => String::new(),
                Value::String(text) => text.clone(),
                other => other.to_string(),
            },
            Some(other) => other.to_string(),
        };
        let kind = schema["type"].as_str().unwrap_or("string");
        if let Some(value) = args.get(name) {
            let fits = match kind {
                "number" | "integer" => value.is_number(),
                "boolean" => value.is_boolean(),
                _ => value.is_string(),
            };
            ensure!(fits, "parameter {name} must be a {kind}");
            if let Some(choices) = schema["enum"].as_array() {
                ensure!(
                    choices.contains(value),
                    "parameter {name} is not one of its choices"
                );
            }
        }
        prompt = prompt.replace(&format!("{{{{{name}}}}}"), &value);
    }
    Ok(prompt)
}

/// `ryzhub-server parse-plugin <namespace>`: read an archive on stdin, write what it holds as
/// JSON on stdout, under memory and CPU limits.
pub fn child_main(args: &[String]) -> anyhow::Result<()> {
    use nix::sys::resource::{Resource, setrlimit};
    setrlimit(Resource::RLIMIT_AS, CHILD_MEMORY, CHILD_MEMORY)?;
    setrlimit(Resource::RLIMIT_CPU, CHILD_CPU_SECS, CHILD_CPU_SECS)?;
    let namespace = args.first().context("namespace")?;
    let mut archive = vec![];
    std::io::stdin()
        .lock()
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut archive)?;
    let reply = match (|| {
        ensure!(
            archive.len() <= MAX_BYTES,
            "package is larger than {MAX_BYTES} bytes"
        );
        let files = unpack(&archive)?;
        let manifest = parse(&files, namespace)?;
        Ok(json!({"manifest": manifest, "content_hash": content_hash(&files, namespace)}))
    })() {
        Ok(value) => value,
        Err(error) => json!({"error": format!("{error:#}")}),
    };
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &reply)?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests;
