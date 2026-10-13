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
    /// This server's address as browsers reach it: OAuth sends them back here.
    pub public_url: String,
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
    pub harness: HarnessConfig,
    pub plugins: PluginsConfig,
    pub auth: AuthConfig,
    pub push: PushConfig,
    pub retention: RetentionConfig,
    pub search: SearchConfig,
    /// Program run as `<program> render-image` to decode uploaded images in a process of its
    /// own; empty runs this server binary.
    pub attachment_renderer: String,
    /// Offers the `ryzhub.v1.json` WebSocket subprotocol, for debugging with text tools.
    /// Development only.
    pub json_protocol: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".into(),
            public_url: "http://127.0.0.1:8080".into(),
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
            harness: HarnessConfig::default(),
            plugins: PluginsConfig::default(),
            auth: AuthConfig::default(),
            push: PushConfig::default(),
            retention: RetentionConfig::default(),
            search: SearchConfig::default(),
            attachment_renderer: String::new(),
            json_protocol: false,
        }
    }
}

/// The node a configuration without `sandbox.nodes` has, at `sandbox.host_socket`.
pub const DEFAULT_NODE: &str = "default";

/// Package registries and source hosts sandboxes reach by default.
pub const DEFAULT_EGRESS: &[&str] = &[
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "index.crates.io",
    "static.crates.io",
    "proxy.golang.org",
    "sum.golang.org",
    "deb.debian.org",
    "security.debian.org",
    "archive.ubuntu.com",
    "security.ubuntu.com",
    "dl-cdn.alpinelinux.org",
    "github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "raw.githubusercontent.com",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub enabled: bool,
    /// Unix socket of the execution node's host agent, when `nodes` is empty.
    pub host_socket: String,
    /// Execution nodes by name. Empty: one node, `default`, at `host_socket`.
    pub nodes: Vec<NodeConfig>,
    /// Hostnames sandboxes may reach through the egress proxy: exact names, or `*.suffix`.
    /// Organization policy can narrow it; empty disables network access.
    pub egress_allow: Vec<String>,
    /// Largest writable layer (and Chat workspace) of a sandbox on a node with a quota
    /// volume; policy `sandbox_disk_bytes` can lower it.
    pub disk_bytes: u64,
    /// How long a Chat sandbox stays up after its run for the next run to reuse (0: it is
    /// destroyed when the run ends). `/workspace` outlives it either way.
    pub chat_idle_secs: u64,
    /// Checkpoint Chat workspaces to storage after runs that used the sandbox, so a workspace
    /// survives the loss of its execution node.
    pub chat_checkpoints: bool,
    /// A stopped Work sandbox (suspended, archived or lost; resuming uses a new one) is
    /// destroyed on its node this long after it stopped.
    pub reclaim_after_secs: u64,
    /// A checkpointed Chat workspace unused this long leaves its node; its next run restores
    /// it (0 keeps it).
    pub workspace_evict_after_secs: u64,
    /// A digest-pinned image nothing references (configuration, sandboxes, checkpoints) is
    /// removed from its node after this long.
    pub image_gc_after_secs: u64,
    /// Base image, pinned by digest.
    pub image: String,
    /// Optional read-only dependency layer over the base image (package caches, common
    /// tools), pinned by digest; `scripts/build-deps.sh` builds one.
    pub deps: String,
    /// Environments a run may choose by name (`env_profile`); without one it gets the image,
    /// layer and resources above.
    pub profiles: Vec<ProfileConfig>,
    /// `runc` or `runsc` (gVisor).
    pub runtime: String,
    pub memory_bytes: u64,
    pub pids_limit: u64,
}

/// A named sandbox environment.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileConfig {
    pub name: String,
    /// Base image, pinned by digest; empty uses `sandbox.image`.
    pub image: String,
    /// Dependency layer, pinned by digest; empty means none.
    pub deps: String,
    pub memory_bytes: Option<u64>,
    pub pids_limit: Option<u64>,
    /// At most this writable layer, within policy.
    pub disk_bytes: Option<u64>,
}

/// An execution node.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    /// Lowercase letters, digits and hyphens; sandboxes record it.
    pub name: String,
    /// Unix socket of its host agent, for a node on this machine.
    pub host_socket: String,
    /// `host:port` of its host agent's TLS listener, for a node elsewhere; then `server_name`
    /// is the name its certificate holds, `tls_ca` the authority it chains to, and
    /// `tls_cert`/`tls_key` this control plane's client certificate.
    pub address: String,
    pub server_name: String,
    pub tls_ca: String,
    pub tls_cert: String,
    pub tls_key: String,
    /// Most sandboxes placed on it at once (0: unlimited).
    pub capacity: u32,
}

impl SandboxConfig {
    /// The configured execution nodes.
    pub fn nodes(&self) -> Vec<NodeConfig> {
        if self.nodes.is_empty() {
            return vec![NodeConfig {
                name: DEFAULT_NODE.into(),
                host_socket: self.host_socket.clone(),
                ..Default::default()
            }];
        }
        self.nodes.clone()
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host_socket: "/run/ryzhub/host.sock".into(),
            nodes: vec![],
            egress_allow: DEFAULT_EGRESS.iter().map(|&host| host.into()).collect(),
            disk_bytes: 10 * 1024 * 1024 * 1024,
            chat_idle_secs: 600,
            chat_checkpoints: true,
            reclaim_after_secs: 24 * 3600,
            workspace_evict_after_secs: 7 * 24 * 3600,
            image_gc_after_secs: 7 * 24 * 3600,
            image: String::new(),
            deps: String::new(),
            profiles: vec![],
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
    /// `anthropic` (Messages), `responses` (OpenAI Responses), `chat_completions` (OpenAI Chat
    /// Completions and compatible) or `gemini` (Gemini generateContent).
    pub protocol: String,
    /// `chat`, `codex` and/or `claude`.
    pub harnesses: Vec<String>,
    /// What the model reads besides text: `image` and/or `pdf`. Files of other kinds, and of
    /// kinds left out here, reach it as paths in the sandbox.
    pub inputs: Vec<String>,
    /// Where the model is served, with the platform's keys: the lowest `priority` first, spread
    /// by `weight` within one. A request moves on to the next when one cannot be reached or
    /// answers 5xx or 429 before anything was forwarded. Empty: the protocol's default
    /// upstream. A member's own credential always goes to the protocol's default upstream.
    pub upstreams: Vec<Upstream>,
}

impl Default for ModelRoute {
    fn default() -> Self {
        Self {
            id: String::new(),
            protocol: "anthropic".into(),
            harnesses: vec!["chat".into()],
            inputs: vec!["image".into(), "pdf".into()],
            upstreams: vec![],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Upstream {
    /// Base URL, without the protocol's path.
    pub url: String,
    /// Environment variable with this upstream's key; empty: the protocol's default.
    pub key_env: String,
    /// The model's id at this upstream, when it differs.
    pub model: String,
    pub priority: u32,
    pub weight: u32,
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            url: String::new(),
            key_env: String::new(),
            model: String::new(),
            priority: 0,
            weight: 1,
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
    /// OpenAI Chat Completions base URL, and its key; models routed elsewhere name their own.
    pub chat_completions_url: String,
    pub chat_completions_key_env: String,
    /// Gemini API base URL (no `/v1beta` suffix), and its key.
    pub gemini_url: String,
    pub gemini_key_env: String,
    pub responses_model: String,
    pub models: Vec<ModelRoute>,
    /// Default Chat model; its protocol follows an explicit `models` route, else Anthropic.
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
    /// Chat requests estimated above this many tokens move their oldest tool results out of
    /// the context (the model reads them back with `read_tool_output`) until they are under
    /// 60% of it. 0 keeps every result.
    pub context_budget_tokens: u32,
    /// Anthropic models, by id prefix, that compact their own context: Chat requests to them
    /// ask for it past `compaction_trigger_tokens`, and `/compact` uses it before falling back
    /// to a summary the model writes.
    pub compaction_models: Vec<String>,
    pub compaction_trigger_tokens: u32,
    /// Anthropic models, by id prefix, that search their tools: Chat runs with many plugin
    /// tools load those on demand (`plugins.defer_tools_above`).
    pub tool_search_models: Vec<String>,
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
            chat_completions_url: "https://api.openai.com".into(),
            chat_completions_key_env: "OPENAI_API_KEY".into(),
            gemini_url: "https://generativelanguage.googleapis.com".into(),
            gemini_key_env: "GEMINI_API_KEY".into(),
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
            context_budget_tokens: 120_000,
            compaction_models: [
                "claude-fable-5",
                "claude-mythos-5",
                "claude-mythos-preview",
                "claude-opus-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-opus-4-6",
                "claude-sonnet-5",
                "claude-sonnet-4-6",
                "claude-haiku-5-5",
            ]
            .map(str::to_owned)
            .to_vec(),
            compaction_trigger_tokens: 150_000,
            tool_search_models: [
                "claude-fable-5",
                "claude-mythos-5",
                "claude-opus-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-opus-4-6",
                "claude-opus-4-5",
                "claude-sonnet-5",
                "claude-sonnet-4-6",
                "claude-sonnet-4-5",
                "claude-haiku-5-5",
                "claude-haiku-4-5",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }
}

/// Work harnesses beyond Codex and Claude Code.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessConfig {
    /// Agents spoken to over the Agent Client Protocol (`[[harness.acp]]`), each a harness of
    /// its own name. Their models are the `llm.models` routes that list the name, plus `model`.
    pub acp: Vec<ryzhub_execution::AcpProfile>,
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
    /// Monthly usage partitions older than this are dropped by `maintain` (0 keeps them).
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
            matches!(self.search.embedding_provider.as_str(), "openai" | "local"),
            "search.embedding_provider must be openai or local"
        );
        ensure!(
            self.search.embedding_provider != "local" || cfg!(feature = "local-embedding"),
            "search.embedding_provider = \"local\" needs a build with the local-embedding feature"
        );
        let mut providers = std::collections::BTreeSet::new();
        for provider in &self.auth.oidc {
            ensure!(
                !provider.name.is_empty()
                    && provider.name != "github"
                    && provider
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    && providers.insert(&provider.name)
                    && reqwest::Url::parse(&provider.issuer).is_ok()
                    && !provider.client_id.is_empty(),
                "invalid OIDC provider {:?}",
                provider.name
            );
        }
        let mut names = std::collections::BTreeSet::new();
        for profile in &self.harness.acp {
            ensure!(
                !profile.name.is_empty()
                    && profile.name.len() <= 32
                    && profile
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    && !matches!(profile.name.as_str(), "chat" | "codex" | "claude")
                    && names.insert(&profile.name),
                "invalid ACP harness name {:?}",
                profile.name
            );
            ensure!(
                std::path::Path::new(&profile.command).is_absolute()
                    && matches!(
                        profile.protocol.as_str(),
                        "anthropic" | "responses" | "chat_completions" | "gemini"
                    )
                    && !profile.model.is_empty()
                    && !profile.version.is_empty(),
                "invalid ACP harness {}",
                profile.name
            );
        }
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
            let upstreams = llm.models.iter().flat_map(|m| &m.upstreams).map(|u| &u.url);
            for url in [
                &llm.upstream_url,
                &llm.gateway_url,
                &llm.responses_url,
                &llm.chat_completions_url,
                &llm.gemini_url,
            ]
            .into_iter()
            .chain(upstreams)
            {
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
            for route in &llm.models {
                ensure!(
                    matches!(
                        route.protocol.as_str(),
                        "anthropic" | "responses" | "chat_completions" | "gemini"
                    ) && route.upstreams.iter().all(|u| u.weight > 0),
                    "invalid route for model {}",
                    route.id
                );
            }
            ensure!(
                llm.compaction_trigger_tokens >= 50_000,
                "llm.compaction_trigger_tokens must be at least 50000"
            );
            if let Some(level) = &self.policy.max_reasoning {
                ensure!(
                    ryzhub_domain::REASONING_LEVELS.contains(&level.as_str()),
                    "invalid policy.max_reasoning"
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
                    && (256 * 1024 * 1024..=1024 * 1024 * 1024 * 1024)
                        .contains(&self.sandbox.disk_bytes),
                "invalid sandbox configuration"
            );
            ensure!(
                self.sandbox.egress_allow.len() <= ryzhub_execution::egress::MAX_RULES
                    && self
                        .sandbox
                        .egress_allow
                        .iter()
                        .all(|rule| ryzhub_execution::egress::valid_rule(rule)),
                "invalid sandbox.egress_allow"
            );
            let pinned = ryzhub_execution::host::pinned;
            ensure!(
                self.sandbox.deps.is_empty() || pinned(&self.sandbox.deps),
                "sandbox.deps must be pinned by digest"
            );
            let mut profiles = std::collections::BTreeSet::new();
            for profile in &self.sandbox.profiles {
                ensure!(
                    profiles.insert(&profile.name)
                        && (1..=63).contains(&profile.name.len())
                        && profile
                            .name
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                        && (profile.image.is_empty() || pinned(&profile.image))
                        && (profile.deps.is_empty() || pinned(&profile.deps))
                        && profile
                            .memory_bytes
                            .is_none_or(|m| (64 << 20..=4 << 30).contains(&m))
                        && profile.pids_limit.is_none_or(|p| (16..=1024).contains(&p))
                        && profile
                            .disk_bytes
                            .is_none_or(|d| (256 << 20..=1 << 40).contains(&d)),
                    "invalid sandbox profile {:?}",
                    profile.name
                );
            }
            let mut names = std::collections::BTreeSet::new();
            for node in self.sandbox.nodes() {
                ensure!(
                    names.insert(node.name.clone())
                        && (1..=63).contains(&node.name.len())
                        && node
                            .name
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                        && !node.name.starts_with('-')
                        && if node.address.is_empty() {
                            std::path::Path::new(&node.host_socket).is_absolute()
                        } else {
                            !node.server_name.is_empty()
                                && [&node.tls_ca, &node.tls_cert, &node.tls_key]
                                    .iter()
                                    .all(|path| std::path::Path::new(path).is_absolute())
                        },
                    "invalid execution node {:?}",
                    node.name
                );
            }
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

/// Signing in with other accounts: OpenID Connect providers and GitHub.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub oidc: Vec<OidcProvider>,
    pub github: Option<GithubProvider>,
    /// An identity whose provider verified its email joins the account with that email.
    pub link_verified_email: bool,
    /// Where clients may ask to return after signing in, by prefix: the web client, and
    /// `http://127.0.0.1:` for native clients listening on loopback.
    pub return_urls: Vec<String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            oidc: vec![],
            github: None,
            link_verified_email: true,
            return_urls: vec![
                "http://127.0.0.1:3000".into(),
                "http://localhost:3000".into(),
                "http://127.0.0.1:".into(),
            ],
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OidcProvider {
    /// Its id in addresses (`/auth/oauth/<name>/…`); lowercase letters, digits and `-`.
    pub name: String,
    pub label: String,
    pub issuer: String,
    pub client_id: String,
    /// Environment variable with the client secret; empty for a public client.
    pub client_secret_env: String,
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubProvider {
    pub label: String,
    pub client_id: String,
    pub client_secret_env: String,
    /// GitHub (or GitHub Enterprise) addresses.
    pub web_url: String,
    pub api_url: String,
}

impl Default for GithubProvider {
    fn default() -> Self {
        Self {
            label: "GitHub".into(),
            client_id: String::new(),
            client_secret_env: "GITHUB_CLIENT_SECRET".into(),
            web_url: "https://github.com".into(),
            api_url: "https://api.github.com".into(),
        }
    }
}

/// Notifications to Android devices through Firebase Cloud Messaging.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PushConfig {
    pub enabled: bool,
    /// The Firebase service account's JSON key file: project, client email, private key and
    /// token endpoint.
    pub fcm_credentials: String,
    /// Where FCM's HTTP v1 API is.
    pub fcm_endpoint: String,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fcm_credentials: String::new(),
            fcm_endpoint: "https://fcm.googleapis.com".into(),
        }
    }
}

/// Plugins (M5): packages and remote MCP servers members install.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    pub enabled: bool,
    /// The platform's extension namespace in packages (`extensions.<namespace>` and the
    /// directory of that name).
    pub namespace: String,
    /// Publishers whose signed packages are trusted: key id → ed25519 public key (base64).
    pub trusted_keys: BTreeMap<String, String>,
    /// The official catalog: a JSON list of `{name, description, source, content_hash}`.
    /// Packages installed from it, with that content, are marked as reviewed.
    pub catalog_url: String,
    /// Hosts reached over plain HTTP and at private addresses. For local development and tests
    /// only: everything else must be HTTPS to a public address.
    pub insecure_hosts: Vec<String>,
    pub fetch_timeout_secs: u64,
    /// Where browsers return after connecting an account.
    pub client_url: String,
    /// MCP Apps are served from this origin, which must differ from the client's; it is
    /// listened on at `content_listen`.
    pub content_origin: String,
    pub content_listen: String,
    /// Chat runs with more plugin tools than this load them on demand (tool search).
    pub defer_tools_above: usize,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            namespace: "app.ryzhub".into(),
            trusted_keys: BTreeMap::new(),
            catalog_url: String::new(),
            insecure_hosts: vec![],
            fetch_timeout_secs: 30,
            client_url: "http://127.0.0.1:3000".into(),
            content_origin: "http://127.0.0.1:8082".into(),
            content_listen: "127.0.0.1:8082".into(),
            defer_tools_above: 30,
        }
    }
}

/// Search is available without embeddings; vector/hybrid modes require a real provider.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub enabled: bool,
    /// `openai` (an OpenAI-compatible endpoint at `embedding_url`) or `local` (a Qwen3
    /// embedding model in this process; needs the `local-embedding` build feature).
    pub embedding_provider: String,
    /// OpenAI-compatible base URL. Empty (with the `openai` provider) means full-text only.
    pub embedding_url: String,
    pub embedding_model: String,
    /// Empty uses the existing OpenAI BYOK/platform credential selection.
    pub embedding_key_env: String,
    /// The local provider's Hugging Face model, fetched once into `HF_HOME` (pre-fill that
    /// cache where the server has no internet).
    pub local_embedding_model: String,
    pub memory_budget_chars: usize,
    pub max_attempts: i32,
}
impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            embedding_provider: "openai".into(),
            embedding_url: String::new(),
            embedding_model: String::new(),
            embedding_key_env: String::new(),
            local_embedding_model: "Qwen/Qwen3-Embedding-0.6B".into(),
            memory_budget_chars: 12000,
            max_attempts: 6,
        }
    }
}
impl SearchConfig {
    /// Whether documents get vectors (else search is full-text only).
    pub fn embeddings(&self) -> bool {
        self.embedding_provider == "local" || !self.embedding_url.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_nodes(nodes: Vec<(&str, &str)>) -> Config {
        let mut config = Config::default();
        config.sandbox.enabled = true;
        config.sandbox.image = "registry.test/base@sha256:00".into();
        config.sandbox.nodes = nodes
            .into_iter()
            .map(|(name, socket)| NodeConfig {
                name: name.into(),
                host_socket: socket.into(),
                ..Default::default()
            })
            .collect();
        config
    }

    #[test]
    fn a_configuration_without_nodes_has_the_default_node() {
        let config = with_nodes(vec![]);
        let nodes = config.sandbox.nodes();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, DEFAULT_NODE);
        assert_eq!(nodes[0].host_socket, config.sandbox.host_socket);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn node_names_are_unique_lowercase_and_sockets_absolute() {
        assert!(
            with_nodes(vec![("a-1", "/s/a"), ("b", "/s/b")])
                .validate()
                .is_ok()
        );
        for bad in [
            vec![("a", "/s/a"), ("a", "/s/b")],
            vec![("Upper", "/s/a")],
            vec![("-lead", "/s/a")],
            vec![("", "/s/a")],
            vec![("a", "relative.sock")],
        ] {
            assert!(with_nodes(bad).validate().is_err());
        }
    }
}
