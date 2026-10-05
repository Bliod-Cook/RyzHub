//! Instance configuration (`RYZHUB_CONFIG`, TOML). Every field has a default suitable for
//! local development; `validate` rejects combinations that cannot work.
use anyhow::ensure;
use ryzhub_domain::Policy;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: String,
    pub allowed_origins: Vec<String>,
    /// Whole weekly event partitions older than this are dropped by `maintain`.
    pub events_retention_days: u32,
    /// Conversation Owner lease; renewed every third of it.
    pub owner_lease_ms: u64,
    /// Simulator stream interval.
    pub mock_chunk_ms: u64,
    /// Pending approvals and questions expire after this long.
    pub approval_ttl_secs: u64,
    /// Instance policy; organizations can only narrow it.
    pub policy: Policy,
    pub llm: LlmConfig,
    pub sandbox: SandboxConfig,
    pub work: WorkConfig,
    pub retention: RetentionConfig,
    pub search: SearchConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".into(),
            allowed_origins: vec![
                "http://localhost:3000".into(),
                "http://127.0.0.1:3000".into(),
            ],
            events_retention_days: 30,
            owner_lease_ms: 3000,
            mock_chunk_ms: 40,
            approval_ttl_secs: 24 * 3600,
            policy: Policy::default(),
            llm: LlmConfig::default(),
            sandbox: SandboxConfig::default(),
            work: WorkConfig::default(),
            retention: RetentionConfig::default(),
            search: SearchConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub enabled: bool,
    /// Unix socket of the execution node's host agent.
    pub host_socket: String,
    /// Base image, pinned by digest.
    pub image: String,
    /// `runc` or `runsc` (gVisor).
    pub runtime: String,
    pub memory_bytes: u64,
    pub pids_limit: u64,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host_socket: "/run/ryzhub/host.sock".into(),
            image: String::new(),
            runtime: "runc".into(),
            memory_bytes: 256 * 1024 * 1024,
            pids_limit: 128,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRoute {
    pub id: String,
    /// `anthropic` (Messages) or `responses` (OpenAI Responses).
    pub protocol: String,
    /// `chat`, `codex` and/or `claude`.
    pub harnesses: Vec<String>,
}

impl Default for ModelRoute {
    fn default() -> Self {
        Self {
            id: String::new(),
            protocol: "anthropic".into(),
            harnesses: vec!["chat".into()],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub enabled: bool,
    pub listen: String,
    /// Where the Owner reaches the gateway.
    pub gateway_url: String,
    /// Anthropic Messages base URL (no `/v1/messages` suffix).
    pub upstream_url: String,
    /// OpenAI Responses base URL.
    pub responses_url: String,
    pub responses_key_env: String,
    pub responses_model: String,
    pub models: Vec<ModelRoute>,
    /// Default Chat model.
    pub model: String,
    pub platform_key_env: String,
    /// Environment variable with the current vault key (32 bytes, URL-safe base64).
    pub vault_key_env: String,
    /// Id recorded with every credential encrypted by `vault_key_env`.
    pub vault_key_id: i16,
    /// Retired vault keys by id; still used to decrypt older credentials.
    pub vault_retired_keys: BTreeMap<String, String>,
    pub requests_per_minute_org: i32,
    pub requests_per_minute_key: i32,
    pub max_tokens: u32,
    pub timeout_secs: u64,
    /// Socket bind-mounted into sandboxes for the guest credential proxy (Work requires it).
    pub unix_socket: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:8081".into(),
            gateway_url: "http://127.0.0.1:8081".into(),
            upstream_url: "https://api.anthropic.com".into(),
            responses_url: "https://api.openai.com".into(),
            responses_key_env: "OPENAI_API_KEY".into(),
            responses_model: String::new(),
            models: vec![],
            model: String::new(),
            platform_key_env: "ANTHROPIC_API_KEY".into(),
            vault_key_env: "RYZHUB_VAULT_KEY".into(),
            vault_key_id: 0,
            vault_retired_keys: BTreeMap::new(),
            requests_per_minute_org: 120,
            requests_per_minute_key: 30,
            max_tokens: 4096,
            timeout_secs: 120,
            unix_socket: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkConfig {
    pub enabled: bool,
    /// Default Claude Code model.
    pub claude_model: String,
    /// Default Codex model.
    pub model: String,
    /// Primary checkpoint store: a directory or `s3config:<path>`.
    pub storage_root: String,
    /// Optional second copy of every checkpoint.
    pub secondary_root: Option<String>,
    /// While a turn runs, checkpoint at least this often.
    pub checkpoint_interval_secs: u64,
    /// Superseded checkpoints older than this are retired (0 keeps them by age).
    pub checkpoint_retention_days: u32,
    /// At most this many checkpoints per conversation are kept (0 disables the bound).
    /// The current head and restore pins are always kept.
    pub checkpoint_keep_last: u32,
    /// Retired checkpoints stay recoverable this long before their blobs are deleted.
    pub checkpoint_gc_grace_secs: u32,
    /// An idle Work sandbox is checkpointed and stopped after this long; the next message
    /// resumes it from that checkpoint (0 keeps sandboxes running).
    pub idle_suspend_secs: u64,
    pub mcp_tools: Vec<crate::mcp::Tool>,
}

impl Default for WorkConfig {
    fn default() -> Self {
        Self {
            claude_model: String::new(),
            enabled: false,
            model: String::new(),
            storage_root: "var/m2/blobs".into(),
            secondary_root: None,
            checkpoint_interval_secs: 300,
            checkpoint_retention_days: 30,
            checkpoint_keep_last: 20,
            checkpoint_gc_grace_secs: 86400,
            idle_suspend_secs: 1800,
            mcp_tools: vec![],
        }
    }
}

/// How long operational records are kept. The Owner role deletes expired rows in batches.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    /// Processed commands (the idempotency window for retried cids).
    pub command_days: u32,
    /// Acknowledged guest events.
    pub guest_event_days: u32,
    /// Expired, revoked or superseded gateway keys.
    pub gateway_key_days: u32,
    /// Usage records (0 keeps them).
    pub usage_days: u32,
    /// Monthly audit partitions older than this are dropped by `maintain` (0 keeps them).
    pub audit_days: u32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            command_days: 7,
            guest_event_days: 7,
            gateway_key_days: 7,
            usage_days: 0,
            audit_days: 400,
        }
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let config: Self = match std::env::var("RYZHUB_CONFIG") {
            Ok(path) => toml::from_str(&std::fs::read_to_string(path)?)?,
            Err(_) => Self::default(),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.events_retention_days >= 7,
            "events retention must be at least one week"
        );
        ensure!(
            self.owner_lease_ms >= 1000 && self.mock_chunk_ms >= 10,
            "invalid owner timing"
        );
        ensure!(
            self.approval_ttl_secs >= 60,
            "approval_ttl_secs is too short"
        );
        ensure!(
            self.retention.command_days >= 1,
            "processed commands must be kept at least one day"
        );
        ensure!(
            self.search.max_attempts > 0 && self.search.memory_budget_chars <= 100_000,
            "invalid search limits"
        );
        if !self.search.embedding_url.is_empty() {
            let url = reqwest::Url::parse(&self.search.embedding_url)?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && !self.search.embedding_model.is_empty(),
                "invalid embedding configuration"
            );
            ensure!(
                !self.search.embedding_key_env.is_empty()
                    || (url.scheme() == "https" && url.host_str() == Some("api.openai.com")),
                "custom embedding_url requires embedding_key_env; personal OpenAI keys are only sent to api.openai.com"
            );
        }
        if self.llm.enabled {
            let llm = &self.llm;
            ensure!(!llm.model.trim().is_empty(), "llm.model required");
            ensure!(
                llm.max_tokens > 0
                    && llm.timeout_secs > 0
                    && llm.requests_per_minute_org > 0
                    && llm.requests_per_minute_key > 0,
                "invalid llm limits"
            );
            for url in [&llm.upstream_url, &llm.gateway_url, &llm.responses_url] {
                let url = reqwest::Url::parse(url)?;
                ensure!(
                    matches!(url.scheme(), "http" | "https")
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none(),
                    "invalid llm endpoint"
                );
            }
            if let Some(path) = &llm.unix_socket {
                ensure!(
                    std::path::Path::new(path).is_absolute() && path.len() < 108,
                    "invalid llm Unix socket path"
                );
            }
        }
        if self.work.enabled {
            ensure!(
                self.llm.enabled
                    && self.sandbox.enabled
                    && self.llm.unix_socket.is_some()
                    && !self.work.model.is_empty()
                    && self.work.checkpoint_interval_secs >= 30,
                "invalid Work configuration"
            );
            let mut names = std::collections::BTreeSet::new();
            for tool in &self.work.mcp_tools {
                let url = reqwest::Url::parse(&tool.url)?;
                ensure!(
                    matches!(url.scheme(), "http" | "https")
                        && url.username().is_empty()
                        && url.password().is_none()
                        && names.insert(&tool.name)
                        && !crate::memory::TOOLS.contains(&tool.name.as_str())
                        && !tool.name.is_empty()
                        && tool.input_schema.is_object(),
                    "invalid MCP catalog"
                );
            }
        }
        if self.sandbox.enabled {
            ensure!(
                self.sandbox.image.contains("@sha256:")
                    && matches!(self.sandbox.runtime.as_str(), "runc" | "runsc")
                    && self.sandbox.memory_bytes >= 32 * 1024 * 1024
                    && self.sandbox.pids_limit > 0
                    && std::path::Path::new(&self.sandbox.host_socket).is_absolute(),
                "invalid sandbox configuration"
            );
        }
        Ok(())
    }

    /// Idle suspension after instance and organization limits.
    pub fn idle_suspend_secs(&self, policy: &Policy) -> u64 {
        match policy.compute_idle_ttl_secs {
            Some(limit) if self.work.idle_suspend_secs == 0 => limit,
            Some(limit) => self.work.idle_suspend_secs.min(limit),
            None => self.work.idle_suspend_secs,
        }
    }
}

/// Search is available without embeddings; vector/hybrid modes require a real provider.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub enabled: bool,
    /// OpenAI-compatible base URL. Empty means full-text only.
    pub embedding_url: String,
    pub embedding_model: String,
    /// Empty uses the existing OpenAI BYOK/platform credential selection.
    pub embedding_key_env: String,
    pub memory_budget_chars: usize,
    pub max_attempts: i32,
}
impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            embedding_url: String::new(),
            embedding_model: String::new(),
            embedding_key_env: String::new(),
            memory_budget_chars: 12000,
            max_attempts: 6,
        }
    }
}
