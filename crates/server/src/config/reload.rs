//! The configuration file, reloaded while the process runs.
//!
//! Every setting takes effect in one of three ways ([`Apply`]). A reload swaps in what the
//! file says, except that restart-only settings keep their running value; those whose file
//! value differs are listed as waiting for a restart. Settings read into a cache when the
//! process starts (node TLS, FCM credentials, the local embedding model) are restart-only,
//! so no cache ever needs invalidating.
use anyhow::Context;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};

use super::{Config, parse_toml};

/// How a change to a setting takes effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Apply {
    /// At once.
    Live,
    /// At once, for runs, sandboxes and pending actions created from then on.
    New,
    /// When the process restarts.
    Restart,
}

/// What a setting, or every setting under a path, is.
pub struct Rule {
    pub path: &'static str,
    /// The admin panel's section it is shown in.
    pub section: &'static str,
    pub apply: Apply,
    /// Whether the panel may change it; otherwise only the file does.
    pub panel: bool,
}

const fn rule(path: &'static str, section: &'static str, apply: Apply, panel: bool) -> Rule {
    Rule {
        path,
        section,
        apply,
        panel,
    }
}

use Apply::{Live, New, Restart};

/// The rule of a path is the longest rule that is the path or a prefix of it.
pub const RULES: &[Rule] = &[
    rule("listen", "general", Restart, false),
    rule("public_url", "general", Live, false),
    rule("allowed_origins", "general", Live, true),
    rule("approval_ttl_secs", "general", New, true),
    rule("attachment_renderer", "general", Live, false),
    rule("auth", "auth", Live, true),
    // A new provider needs its secret in the environment, and so a restart; another issuer
    // under an existing name would sign people in to the wrong accounts.
    rule("auth.oidc", "auth", Live, false),
    rule("auth.github", "auth", Live, false),
    rule("llm", "models", Live, true),
    rule("llm.enabled", "models", Restart, true),
    rule("llm.listen", "models", Restart, false),
    rule("llm.gateway_url", "models", Live, false),
    rule("llm.unix_socket", "models", Restart, false),
    rule("llm.vault_key_env", "models", Restart, false),
    rule("llm.vault_key_id", "models", Restart, false),
    rule("llm.vault_retired_keys", "models", Restart, false),
    rule("llm.model", "models", New, true),
    rule("policy", "policy", Live, true),
    // Images, runtime, limits and egress apply to sandboxes started from then on.
    rule("sandbox", "sandbox", New, true),
    rule("sandbox.enabled", "sandbox", Restart, true),
    rule("sandbox.host_socket", "sandbox", Restart, false),
    rule("sandbox.nodes", "sandbox", Restart, false),
    // Whether the checkpoint collector runs is decided at start.
    rule("sandbox.chat_checkpoints", "sandbox", Restart, true),
    rule("sandbox.reclaim_after_secs", "sandbox", Live, true),
    rule("sandbox.workspace_evict_after_secs", "sandbox", Live, true),
    rule("sandbox.image_gc_after_secs", "sandbox", Live, true),
    rule("work", "work", Live, true),
    rule("work.enabled", "work", Restart, true),
    rule("work.model", "work", New, true),
    rule("work.claude_model", "work", New, true),
    // Read on every use: switching would strand the stored objects until they are moved.
    rule("work.storage_root", "work", Restart, false),
    rule("work.secondary_root", "work", Restart, false),
    rule("work.checkpoint_retention_days", "retention", Live, true),
    rule("work.checkpoint_keep_last", "retention", Live, true),
    rule("work.checkpoint_gc_grace_secs", "retention", Live, true),
    rule("harness", "work", New, true),
    rule("plugins", "plugins", Live, true),
    rule("plugins.enabled", "plugins", Restart, true),
    rule("plugins.content_listen", "plugins", Restart, false),
    rule("plugins.insecure_hosts", "plugins", Live, false),
    rule("search", "search", Live, true),
    rule("search.enabled", "search", Restart, true),
    rule("search.local_embedding_model", "search", Restart, false),
    rule("push", "push", Live, true),
    rule("push.fcm_credentials", "push", Restart, false),
    rule("push.fcm_endpoint", "push", Live, false),
    rule("events_retention_days", "retention", Live, true),
    rule("retention", "retention", Live, true),
];

pub fn rule_of(path: &str) -> Option<&'static Rule> {
    RULES
        .iter()
        .filter(|rule| {
            path == rule.path
                || path
                    .strip_prefix(rule.path)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
        .max_by_key(|rule| rule.path.len())
}

/// Tables that are one setting: keys are data, not settings.
const WHOLE: &[&str] = &[
    "auth.github",
    "llm.vault_retired_keys",
    "plugins.trusted_keys",
];

/// Every setting of `config` by dotted path. Lists, including lists of tables, are one setting.
pub fn settings(config: &Config) -> BTreeMap<String, Value> {
    fn walk(prefix: &str, value: &Value, out: &mut BTreeMap<String, Value>) {
        match value {
            Value::Object(fields) if !WHOLE.contains(&prefix) => {
                for (key, value) in fields {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(&path, value, out);
                }
            }
            _ => {
                out.insert(prefix.to_owned(), value.clone());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(
        "",
        &serde_json::to_value(config).expect("configuration as JSON"),
        &mut out,
    );
    out
}

/// What to run with when the file says `text` while `running` runs: the file's configuration
/// with every restart-only setting at its running value (None when that combination is
/// invalid), and the settings that wait for a restart (all that changed, when None).
pub fn reconcile(running: &Config, text: &str) -> anyhow::Result<(Option<Config>, Vec<String>)> {
    let file: Config = parse_toml(text)?;
    file.validate()?;
    let (now, wanted) = (settings(running), settings(&file));
    let changed: Vec<&String> = wanted
        .keys()
        .filter(|p| now.get(*p) != wanted.get(*p))
        .collect();
    let pending: Vec<String> = changed
        .iter()
        .filter(|path| rule_of(path).is_some_and(|rule| rule.apply == Restart))
        .map(|path| path.to_string())
        .collect();
    let mut merged = serde_json::to_value(&file)?;
    for path in &pending {
        let pointer = format!("/{}", path.replace('.', "/"));
        *merged.pointer_mut(&pointer).context("setting")? = now[path].clone();
    }
    let merged: Config = serde_json::from_value(merged)?;
    if merged.validate().is_err() {
        return Ok((None, changed.into_iter().cloned().collect()));
    }
    Ok((Some(merged), pending))
}

/// A configuration file's version: the SHA-256 of its content.
pub fn version(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// How the last reload went.
#[derive(Clone, Debug, Default)]
pub struct Status {
    /// The version of the file last read.
    pub version: Option<String>,
    /// Settings whose file value waits for a restart.
    pub pending: Vec<String>,
    /// Why the file last read was not used.
    pub error: Option<String>,
}

/// The running configuration and the file it comes from.
pub struct Running {
    current: RwLock<Arc<Config>>,
    pub path: Option<PathBuf>,
    status: Mutex<Status>,
}

impl Running {
    /// Running `config`, read from `path` if it came from a file.
    pub fn new(config: Config, path: Option<PathBuf>) -> Self {
        let version = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| version(&text));
        Self {
            current: RwLock::new(Arc::new(config)),
            path,
            status: Mutex::new(Status {
                version,
                ..Default::default()
            }),
        }
    }

    /// The configuration in `RYZHUB_CONFIG`, or the defaults without one.
    pub fn open() -> anyhow::Result<Self> {
        let path = std::env::var_os("RYZHUB_CONFIG").map(PathBuf::from);
        Ok(Self::new(Config::load()?, path))
    }

    pub fn current(&self) -> Arc<Config> {
        self.current.read().unwrap().clone()
    }

    pub fn set(&self, config: Config) {
        *self.current.write().unwrap() = Arc::new(config);
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    /// Read the file again and run with it, if it changed since it was last read.
    pub fn reload(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let text = std::fs::read_to_string(path)?;
        let version = version(&text);
        if self.status().version.as_ref() == Some(&version) {
            return Ok(());
        }
        let running = self.current();
        let mut status = Status {
            version: Some(version),
            ..self.status()
        };
        match reconcile(&running, &text) {
            Ok((merged, pending)) => {
                if let Some(merged) = merged {
                    self.set(merged);
                }
                status.pending = pending;
                status.error = None;
                tracing::info!(pending = ?status.pending, "configuration reloaded");
            }
            Err(error) => {
                tracing::error!(error = format!("{error:#}"), "configuration not reloaded");
                status.error = Some(format!("{error:#}"));
            }
        }
        *self.status.lock().unwrap() = status;
        Ok(())
    }
}

/// Reload the configuration file whenever it changes, checking every few seconds.
pub async fn watch(running: Arc<Running>) -> anyhow::Result<()> {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let running = running.clone();
        tokio::task::spawn_blocking(move || running.reload()).await??;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandboxed() -> Config {
        let mut config = Config::default();
        config.sandbox.enabled = true;
        config.sandbox.image = format!("base@sha256:{}", "0".repeat(64));
        config
    }

    #[test]
    fn every_setting_has_a_rule() {
        let mut config = Config::default();
        config.auth.github = Some(Default::default());
        for path in settings(&config).keys() {
            assert!(rule_of(path).is_some(), "{path}");
        }
        assert_eq!(rule_of("sandbox.memory_bytes").unwrap().apply, Apply::New);
        assert_eq!(rule_of("sandbox.nodes").unwrap().apply, Apply::Restart);
        assert!(rule_of("policy.models").unwrap().panel);
        assert!(rule_of("listenx").is_none());
    }

    #[test]
    fn restart_settings_keep_their_running_value_and_wait() {
        let running = Config::default();
        let text = "allowed_origins = [\"https://a.test\"]\n[llm]\nlisten = \"127.0.0.1:9\"\n";
        let (merged, pending) = reconcile(&running, text).unwrap();
        let merged = merged.unwrap();
        assert_eq!(merged.allowed_origins, ["https://a.test"]);
        assert_eq!(merged.llm.listen, running.llm.listen);
        assert_eq!(pending, ["llm.listen"]);
        // Back as it runs, nothing waits.
        assert!(reconcile(&running, "").unwrap().1.is_empty());
    }

    #[test]
    fn an_invalid_file_is_refused() {
        let error = reconcile(&Config::default(), "events_retention_days = 1").unwrap_err();
        assert!(error.to_string().contains("one week"), "{error}");
        assert!(reconcile(&Config::default(), "listne = \"x\"").is_err());
    }

    #[test]
    fn a_change_invalid_next_to_running_values_waits_for_a_restart_whole() {
        // Turning sandboxes off and dropping their image is valid in the file, but the
        // running process keeps sandboxes on, which needs the image.
        let running = sandboxed();
        let text = "[sandbox]\nenabled = false\nimage = \"\"\nchat_idle_secs = 5\n";
        let (merged, mut pending) = reconcile(&running, text).unwrap();
        assert!(merged.is_none());
        pending.sort();
        assert_eq!(
            pending,
            ["sandbox.chat_idle_secs", "sandbox.enabled", "sandbox.image"]
        );
    }

    #[test]
    fn a_reload_reads_the_file_only_when_it_changed() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, b"approval_ttl_secs = 120\n").unwrap();
        let live = Running::new(Config::default(), Some(file.path().into()));
        live.reload().unwrap();
        // The version is the file's, read when the process started.
        assert_eq!(
            live.current().approval_ttl_secs,
            Config::default().approval_ttl_secs
        );
        std::fs::write(file.path(), "approval_ttl_secs = 1\n").unwrap();
        live.reload().unwrap();
        assert!(live.status().error.is_some());
        std::fs::write(file.path(), "approval_ttl_secs = 120\n").unwrap();
        live.reload().unwrap();
        assert_eq!(live.current().approval_ttl_secs, 120);
        assert_eq!(live.status().error, None);
    }
}
