use ryzhub_domain::Policy;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: String,
    pub allowed_origins: Vec<String>,
    pub events_retention_days: u32,
    pub owner_lease_ms: u64,
    pub mock_chunk_ms: u64,
    pub policy: Policy,
    pub llm: LlmConfig,
    pub sandbox: SandboxConfig,
    pub work: WorkConfig,
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
            policy: Policy::default(),
            llm: LlmConfig::default(),
            sandbox: SandboxConfig::default(),
            work: WorkConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub host_socket: String,
    pub image: String,
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
impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let value: Self = if let Ok(path) = std::env::var("RYZHUB_CONFIG") {
            toml::from_str(&std::fs::read_to_string(path)?)?
        } else {
            Self::default()
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.events_retention_days >= 7,
            "events retention must be at least one week"
        );
        anyhow::ensure!(
            self.owner_lease_ms >= 1000 && self.mock_chunk_ms >= 10,
            "invalid owner timing"
        );
        if self.llm.enabled {
            anyhow::ensure!(!self.llm.model.trim().is_empty(), "llm.model required");
            anyhow::ensure!(
                self.llm.max_tokens > 0
                    && self.llm.timeout_secs > 0
                    && self.llm.requests_per_minute_org > 0
                    && self.llm.requests_per_minute_key > 0,
                "invalid llm limits"
            );
            for url in [
                &self.llm.upstream_url,
                &self.llm.gateway_url,
                &self.llm.responses_url,
            ] {
                let url = reqwest::Url::parse(url)?;
                anyhow::ensure!(
                    matches!(url.scheme(), "http" | "https")
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none(),
                    "invalid llm endpoint"
                );
            }
            if let Some(path) = &self.llm.unix_socket {
                anyhow::ensure!(
                    std::path::Path::new(path).is_absolute() && path.len() < 108,
                    "invalid llm Unix socket path"
                );
            }
        }
        if self.work.enabled {
            anyhow::ensure!(
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
                anyhow::ensure!(
                    matches!(url.scheme(), "http" | "https")
                        && url.username().is_empty()
                        && url.password().is_none()
                        && names.insert(&tool.name)
                        && !tool.name.is_empty()
                        && tool.input_schema.is_object(),
                    "invalid MCP catalog"
                );
            }
        }
        if self.sandbox.enabled {
            anyhow::ensure!(
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRoute {
    pub id: String,
    pub protocol: String,
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
    pub gateway_url: String,
    pub upstream_url: String,
    pub responses_url: String,
    pub responses_key_env: String,
    pub responses_model: String,
    pub models: Vec<ModelRoute>,
    pub model: String,
    pub platform_key_env: String,
    pub vault_key_env: String,
    pub signing_key_env: String,
    pub requests_per_minute_org: i32,
    pub requests_per_minute_key: i32,
    pub max_tokens: u32,
    pub timeout_secs: u64,
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
            signing_key_env: "RYZHUB_SIGNING_KEY".into(),
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
    pub claude_model: String,
    pub model: String,
    pub storage_root: String,
    pub secondary_root: Option<String>,
    pub checkpoint_interval_secs: u64,
    /// Zero retains superseded checkpoints indefinitely. Current heads are always protected.
    pub checkpoint_retention_days: u32,
    pub checkpoint_gc_grace_secs: u32,
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
            checkpoint_retention_days: 0,
            checkpoint_gc_grace_secs: 86400,
            mcp_tools: vec![],
        }
    }
}
