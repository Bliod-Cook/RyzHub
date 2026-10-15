//! Validated per-conversation settings. Each run receives an immutable snapshot.
use anyhow::ensure;
use ryzhub_domain::Policy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    config::Config,
    error::Code,
    store::{Tx, effective_policy},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunConfig {
    pub harness: String,
    pub protocol: String,
    pub model: String,
    pub reasoning: String,
    pub approval: String,
    pub plan: bool,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            harness: String::new(),
            // Empty means "follow the model": `resolve` infers it from the catalog.
            protocol: String::new(),
            model: String::new(),
            reasoning: "medium".into(),
            approval: "ask".into(),
            plan: false,
        }
    }
}

impl RunConfig {
    pub fn resolve(value: Value, mode: &str, config: &Config) -> anyhow::Result<Self> {
        let mut c: Self = serde_json::from_value(value)?;
        let catalog = catalog(config);
        if c.harness.is_empty() {
            // A Work model belongs to exactly one native harness.
            c.harness = if mode != "work" {
                "chat".into()
            } else {
                catalog
                    .iter()
                    .filter(|m| m.id == c.model)
                    .flat_map(|m| &m.harnesses)
                    .find(|h| matches!(h.as_str(), "codex" | "claude") || acp(config, h).is_some())
                    .cloned()
                    .unwrap_or_else(|| "codex".into())
            };
        }
        let profile = acp(config, &c.harness);
        if c.model.is_empty() {
            c.model = match (c.harness.as_str(), profile) {
                (_, Some(profile)) => profile.model.clone(),
                ("codex", _) => config.work.model.clone(),
                ("claude", _) => config.work.claude_model.clone(),
                _ => config.llm.model.clone(),
            };
        }
        // An ACP agent's provider speaks one protocol.
        if let Some(profile) = profile {
            c.protocol = profile.protocol.clone();
        }
        if c.protocol.is_empty() {
            // Resolve the default model first, then follow its catalog route.
            c.protocol = catalog
                .iter()
                .find(|m| m.id == c.model && m.harnesses.contains(&c.harness))
                .map_or_else(|| "anthropic".into(), |m| m.protocol.clone());
        }
        ensure!(
            matches!(
                (mode, c.harness.as_str()),
                ("chat", "chat") | ("work", "codex" | "claude")
            ) || (mode == "work" && profile.is_some()),
            Code::InvalidHarness
        );
        ensure!(
            profile.is_some() || matches!(c.protocol.as_str(), "anthropic" | "responses"),
            Code::InvalidProtocol
        );
        if c.harness == "codex" {
            c.protocol = "responses".into();
        }
        if c.harness == "claude" {
            c.protocol = "anthropic".into();
        }
        ensure!(
            matches!(c.approval.as_str(), "ask" | "allow_edits" | "full_auto"),
            Code::InvalidApprovalMode
        );
        ensure!(
            matches!(
                c.reasoning.as_str(),
                "low" | "medium" | "high" | "xhigh" | "max"
            ),
            Code::InvalidReasoning
        );
        ensure!(!c.plan || mode == "work", Code::PlanUnsupported);
        ensure!(
            !(c.harness == "codex" && c.reasoning == "max"),
            Code::InvalidReasoning
        );
        Ok(c)
    }

    /// The settings may be used now: their engine is on, policy allows them and their model
    /// is routed. Returns the organization's effective policy.
    pub async fn validate(
        &self,
        tx: &mut Tx<'_>,
        org: Uuid,
        config: &Config,
    ) -> anyhow::Result<Policy> {
        ensure!(
            self.harness == "chat" || config.work.enabled,
            Code::WorkDisabled
        );
        let policy = effective_policy(tx, &config.policy, org).await?;
        ensure!(self.allowed(&policy), Code::PolicyDenied);
        ensure!(
            self.approval != "full_auto" || policy.allow_full_auto != Some(false),
            Code::PolicyDenied
        );
        // Catalog entries are explicit routes. Legacy default models remain routable.
        let routes = models(config, &self.harness, &self.protocol);
        ensure!(routes.contains(&self.model), Code::ModelNotRoutable);
        Ok(policy)
    }

    /// Policy allows the model and the harness.
    pub fn allowed(&self, policy: &Policy) -> bool {
        policy
            .models
            .as_ref()
            .is_none_or(|m| m.contains(&self.model))
            && policy
                .harnesses
                .as_ref()
                .is_none_or(|h| h.contains(&self.harness))
    }
}

pub fn models(config: &Config, harness: &str, protocol: &str) -> Vec<String> {
    catalog(config)
        .into_iter()
        .filter(|m| m.protocol == protocol && m.harnesses.iter().any(|h| h == harness))
        .map(|m| m.id)
        .collect()
}

/// The ACP agent behind `harness`, when it is one.
pub fn acp<'a>(config: &'a Config, harness: &str) -> Option<&'a ryzhub_execution::AcpProfile> {
    config.harness.acp.iter().find(|p| p.name == harness)
}

/// The Work harnesses the instance offers.
pub fn work_harnesses(config: &Config) -> Vec<&str> {
    let mut harnesses = vec!["codex"];
    if !config.work.claude_model.is_empty() {
        harnesses.push("claude");
    }
    harnesses.extend(config.harness.acp.iter().map(|p| p.name.as_str()));
    harnesses
}

/// The name clients show for a Work harness.
pub fn harness_label<'a>(config: &'a Config, harness: &'a str) -> &'a str {
    match (harness, acp(config, harness)) {
        (_, Some(profile)) if !profile.label.is_empty() => &profile.label,
        ("codex", _) => "Codex",
        ("claude", _) => "Claude Code",
        _ => harness,
    }
}

/// The harness version a checkpoint records; a restore requires the same.
pub fn harness_version(config: &Config, harness: &str) -> String {
    acp(config, harness).map_or_else(
        || ryzhub_execution::native::harness_version(harness).to_owned(),
        |profile| profile.version.clone(),
    )
}

/// An explicit route can make the default Chat model use Responses.
pub fn default_chat_protocol(config: &Config) -> &str {
    config
        .llm
        .models
        .iter()
        .find(|m| m.id == config.llm.model && m.harnesses.iter().any(|h| h == "chat"))
        .map_or("anthropic", |m| m.protocol.as_str())
}

/// A routable model: explicit `llm.models` routes plus the per-path defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelEntry {
    pub id: String,
    pub protocol: String,
    pub harnesses: Vec<String>,
}

/// The routable models. Work harnesses only while Work is enabled.
pub fn catalog(config: &Config) -> Vec<ModelEntry> {
    let defaults = [
        (&config.llm.model, default_chat_protocol(config), "chat"),
        (&config.work.model, "responses", "codex"),
        (&config.work.claude_model, "anthropic", "claude"),
    ];
    let agents = config
        .harness
        .acp
        .iter()
        .map(|p| (&p.model, p.protocol.as_str(), p.name.as_str()));
    let routes = config
        .llm
        .models
        .iter()
        .map(|m| (&m.id, m.protocol.as_str(), m.harnesses.clone()))
        .chain(
            defaults
                .into_iter()
                .chain(agents)
                .map(|(id, protocol, harness)| (id, protocol, vec![harness.to_owned()])),
        );
    let mut out: Vec<ModelEntry> = vec![];
    for (id, protocol, mut harnesses) in routes {
        harnesses.retain(|h| h == "chat" || config.work.enabled);
        if id.is_empty() || harnesses.is_empty() {
            continue;
        }
        match out
            .iter_mut()
            .find(|m| &m.id == id && m.protocol == protocol)
        {
            Some(m) => {
                for h in harnesses {
                    if !m.harnesses.contains(&h) {
                        m.harnesses.push(h);
                    }
                }
            }
            None => out.push(ModelEntry {
                id: id.clone(),
                protocol: protocol.to_owned(),
                harnesses,
            }),
        }
    }
    out
}

/// What `model` reads besides text. Models without an explicit route read images and PDFs.
pub fn inputs(config: &Config, model: &str) -> Vec<String> {
    config
        .llm
        .models
        .iter()
        .find(|route| route.id == model)
        .map_or_else(
            || vec!["image".into(), "pdf".into()],
            |route| route.inputs.clone(),
        )
}

/// The provider behind a wire protocol; also the gateway credential provider.
pub fn provider(protocol: &str) -> &'static str {
    if protocol == "responses" {
        "openai"
    } else {
        "anthropic"
    }
}

pub fn reasoning_levels(harness: &str, protocol: &str) -> Vec<&'static str> {
    match (harness, protocol) {
        ("claude", _) => vec!["low", "medium", "high", "xhigh", "max"],
        ("codex", _) | ("chat", "responses") => vec!["low", "medium", "high", "xhigh"],
        // Chat over Messages, and ACP agents, which choose their own.
        _ => vec![],
    }
}

/// Feature support by harness and protocol, as clients render it: `native`, `emulated` or
/// `unsupported`.
pub fn capabilities(harness: &str, protocol: &str) -> Value {
    let mut c = json!({
        "steer": "native",
        "fork": "native",
        "compact": "native",
        "plan_mode": "native",
        "thought_level": "native",
        "terminal": "native",
    });
    if harness == "claude" {
        c["steer"] = json!("emulated");
    }
    if !matches!(harness, "chat" | "codex" | "claude") {
        // An agent over ACP: no steering (steers wait for the next turn), forks or reasoning
        // control.
        c["steer"] = json!("emulated");
        c["fork"] = json!("unsupported");
        c["thought_level"] = json!("unsupported");
    }
    if harness == "chat" {
        c["terminal"] = json!("unsupported");
        c["steer"] = json!("emulated");
        c["plan_mode"] = json!("unsupported");
        // Every Chat context compacts: by the provider where it can, else by a summary.
        if protocol != "responses" {
            c["thought_level"] = json!("unsupported");
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelRoute;

    fn config() -> Config {
        let mut c = Config::default();
        c.llm.enabled = true;
        c.llm.model = "fixture-model".into();
        c.llm.unix_socket = Some("/run/ryzhub/llm.sock".into());
        c.sandbox.enabled = true;
        c.sandbox.image = "registry.test/base@sha256:00".into();
        c.work.enabled = true;
        c.work.model = "gpt-5.4".into();
        c.work.claude_model = "claude-sonnet-4-6".into();
        c.llm.models = vec![
            ModelRoute {
                id: "gpt-5.2".into(),
                protocol: "responses".into(),
                harnesses: vec!["codex".into(), "chat".into()],
                ..Default::default()
            },
            ModelRoute {
                id: "claude-opus-4-6".into(),
                protocol: "anthropic".into(),
                harnesses: vec!["claude".into(), "chat".into()],
                ..Default::default()
            },
        ];
        c
    }

    #[test]
    fn catalog_merges_routes_and_defaults() {
        let mut merged = config();
        merged.work.model = "gpt-5.2".into();
        let c = catalog(&merged);
        let gpt = c.iter().find(|m| m.id == "gpt-5.2").unwrap();
        assert_eq!(gpt.protocol, "responses");
        assert_eq!(gpt.harnesses, ["codex", "chat"]);
        assert_eq!(c.iter().filter(|m| m.id == "gpt-5.2").count(), 1);
        assert!(
            c.iter()
                .any(|m| m.id == "fixture-model" && m.harnesses == ["chat"])
        );
        // Without Work, its harnesses and Work-only models are not offered.
        let mut chat_only = config();
        chat_only.work.enabled = false;
        let c = catalog(&chat_only);
        assert!(c.iter().all(|m| m.harnesses == ["chat"]));
        assert!(!c.iter().any(|m| m.id == "gpt-5.4"));
    }

    #[test]
    fn protocol_follows_the_chosen_model() {
        let c = config();
        let chat = |v| RunConfig::resolve(v, "chat", &c).unwrap();
        let r = chat(json!({"model":"gpt-5.2"}));
        assert_eq!(
            (r.harness.as_str(), r.protocol.as_str()),
            ("chat", "responses")
        );
        assert_eq!(
            chat(json!({"model":"claude-opus-4-6"})).protocol,
            "anthropic"
        );
        let d = chat(json!({}));
        assert_eq!(
            (d.protocol.as_str(), d.model.as_str()),
            ("anthropic", "fixture-model")
        );
    }

    #[test]
    fn responses_default_chat_has_one_route_and_resolves_without_a_model() {
        let mut c = config();
        c.llm.model = "gpt-5.2".into();
        let entries = catalog(&c);
        let defaults: Vec<_> = entries.iter().filter(|m| m.id == c.llm.model).collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].protocol, "responses");
        assert_eq!(default_chat_protocol(&c), "responses");
        assert!(!models(&c, "chat", "anthropic").contains(&c.llm.model));
        assert!(models(&c, "chat", "responses").contains(&c.llm.model));

        let resolved = RunConfig::resolve(json!({}), "chat", &c).unwrap();
        assert_eq!(resolved.model, "gpt-5.2");
        assert_eq!(resolved.protocol, "responses");
    }

    #[test]
    fn work_harness_follows_the_model_when_omitted() {
        let c = config();
        let work = |v| RunConfig::resolve(v, "work", &c).unwrap();
        let claude = work(json!({"model":"claude-opus-4-6"}));
        assert_eq!(
            (claude.harness.as_str(), claude.protocol.as_str()),
            ("claude", "anthropic")
        );
        assert_eq!(work(json!({"model":"gpt-5.2"})).harness, "codex");
        assert_eq!(work(json!({})).model, "gpt-5.4");
        let explicit = work(json!({"harness":"claude"}));
        assert_eq!(explicit.model, "claude-sonnet-4-6");
    }

    #[test]
    fn acp_agents_are_work_harnesses_of_their_profiles() {
        let mut c = config();
        c.harness.acp = vec![ryzhub_execution::AcpProfile {
            name: "opencode".into(),
            label: "OpenCode".into(),
            command: "/usr/local/bin/opencode".into(),
            protocol: "chat_completions".into(),
            model: "glm-5".into(),
            version: "1.19.0".into(),
            ..Default::default()
        }];
        c.llm.models[1].harnesses.push("opencode".into());
        c.validate().unwrap();
        let work = |v| RunConfig::resolve(v, "work", &c);
        let agent = work(json!({"harness":"opencode"})).unwrap();
        // Its provider's protocol, whatever the model's route says.
        assert_eq!(
            (agent.model.as_str(), agent.protocol.as_str()),
            ("glm-5", "chat_completions")
        );
        assert_eq!(work(json!({"model":"glm-5"})).unwrap().harness, "opencode");
        assert_eq!(
            models(&c, "opencode", "chat_completions"),
            vec!["glm-5".to_owned()]
        );
        assert!(RunConfig::resolve(json!({"harness":"opencode"}), "chat", &c).is_err());
        assert_eq!(work_harnesses(&c), ["codex", "claude", "opencode"]);
        assert_eq!(harness_label(&c, "opencode"), "OpenCode");
        assert_eq!(harness_version(&c, "opencode"), "1.19.0");
        let caps = capabilities("opencode", "chat_completions");
        assert_eq!(
            (
                caps["steer"].as_str(),
                caps["fork"].as_str(),
                caps["compact"].as_str()
            ),
            (Some("emulated"), Some("unsupported"), Some("native"))
        );
        assert!(reasoning_levels("opencode", "chat_completions").is_empty());
        for name in ["codex", "Open Code", ""] {
            let mut bad = c.clone();
            bad.harness.acp[0].name = name.into();
            assert!(bad.validate().is_err(), "{name:?}");
        }
    }

    #[test]
    fn unknown_models_keep_the_legacy_default_protocol() {
        let c = config();
        let r = RunConfig::resolve(json!({"model":"unknown"}), "chat", &c).unwrap();
        assert_eq!(r.protocol, "anthropic");
        assert!(!models(&c, &r.harness, &r.protocol).contains(&r.model));
    }

    #[test]
    fn invalid_settings_are_rejected_with_stable_codes() {
        let c = config();
        let code = |v, mode| crate::error::code_of(&RunConfig::resolve(v, mode, &c).unwrap_err());
        assert_eq!(
            code(json!({"harness":"codex"}), "chat"),
            Some(Code::InvalidHarness)
        );
        assert_eq!(
            code(json!({"approval":"yolo"}), "chat"),
            Some(Code::InvalidApprovalMode)
        );
        assert_eq!(
            code(json!({"plan":true}), "chat"),
            Some(Code::PlanUnsupported)
        );
    }

    #[test]
    fn reasoning_levels_per_path() {
        assert!(reasoning_levels("chat", "anthropic").is_empty());
        assert_eq!(reasoning_levels("chat", "responses").last(), Some(&"xhigh"));
        assert_eq!(reasoning_levels("claude", "anthropic").last(), Some(&"max"));
        assert_eq!(provider("responses"), "openai");
    }
}
