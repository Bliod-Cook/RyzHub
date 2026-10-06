//! Validated per-conversation settings. Each run receives an immutable snapshot.
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{config::Config, error::Code, store::Tx};

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
                    .find(|h| matches!(h.as_str(), "codex" | "claude"))
                    .cloned()
                    .unwrap_or_else(|| "codex".into())
            };
        }
        if c.model.is_empty() && !config.llm.enabled {
            c.model = "mock".into();
        }
        if c.model.is_empty() {
            c.model = match c.harness.as_str() {
                "codex" => config.work.model.clone(),
                "claude" => config.work.claude_model.clone(),
                _ if c.protocol == "responses" => config.llm.responses_model.clone(),
                _ => config.llm.model.clone(),
            };
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
            ),
            Code::InvalidHarness
        );
        ensure!(
            matches!(c.protocol.as_str(), "anthropic" | "responses"),
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

    pub async fn validate(
        &self,
        tx: &mut Tx<'_>,
        org: Uuid,
        config: &Config,
    ) -> anyhow::Result<()> {
        let org_policy: Option<Value> = sqlx::query_scalar(
            "SELECT policy FROM org_policies WHERE org_id=$1 ORDER BY version DESC LIMIT 1",
        )
        .bind(org)
        .fetch_optional(&mut **tx)
        .await?;
        let p = config
            .policy
            .restrict(&serde_json::from_value(org_policy.unwrap_or(json!({})))?);
        ensure!(
            p.models.as_ref().is_none_or(|m| m.contains(&self.model))
                && p.harnesses
                    .as_ref()
                    .is_none_or(|h| h.contains(&self.harness)),
            Code::PolicyDenied
        );
        ensure!(
            self.approval != "full_auto" || p.allow_full_auto != Some(false),
            Code::PolicyDenied
        );
        // Catalog entries are explicit routes. Legacy default models remain routable.
        let routes = models(config, &self.harness, &self.protocol);
        ensure!(routes.contains(&self.model), Code::ModelNotRoutable);
        Ok(())
    }
}

pub fn models(config: &Config, harness: &str, protocol: &str) -> Vec<String> {
    if !config.llm.enabled {
        return vec!["mock".into()];
    }
    catalog(config)
        .into_iter()
        .filter(|m| m.protocol == protocol && m.harnesses.iter().any(|h| h == harness))
        .map(|m| m.id)
        .collect()
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

pub fn catalog(config: &Config) -> Vec<ModelEntry> {
    if !config.llm.enabled {
        return vec![ModelEntry {
            id: "mock".into(),
            protocol: "anthropic".into(),
            harnesses: vec!["chat".into(), "codex".into()],
        }];
    }
    let defaults = [
        (&config.llm.model, default_chat_protocol(config), "chat"),
        (&config.llm.responses_model, "responses", "chat"),
        (&config.work.model, "responses", "codex"),
        (&config.work.claude_model, "anthropic", "claude"),
    ];
    let routes = config
        .llm
        .models
        .iter()
        .map(|m| (&m.id, m.protocol.as_str(), m.harnesses.clone()))
        .chain(
            defaults
                .into_iter()
                .map(|(id, protocol, harness)| (id, protocol, vec![harness.to_owned()])),
        );
    let mut out: Vec<ModelEntry> = vec![];
    for (id, protocol, harnesses) in routes {
        if id.is_empty() {
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
        ("chat", p) if p != "responses" => vec![],
        _ => vec!["low", "medium", "high", "xhigh"],
    }
}

/// Feature support by harness and protocol, as clients render it: `native`, `emulated` or
/// `unsupported`.
pub fn capabilities(harness: &str, protocol: &str) -> Value {
    let mut c = json!({
        "steer": "native",
        "interrupt": "native",
        "approval_granularity": "native",
        "ask_user": "native",
        "resume": "native",
        "fork": "native",
        "compact": "native",
        "plan_mode": "native",
        "model_switch": "native",
        "thought_level": "native",
        "commands_list": "native",
        "todo": "unsupported",
        "diff": "unsupported",
        "terminal_stream": "unsupported",
        "subagent": "unsupported",
        "durable_pause": "emulated",
        "side_query": "emulated",
    });
    if harness == "claude" {
        c["steer"] = json!("emulated");
        c["todo"] = json!("native");
        c["subagent"] = json!("native");
    }
    if harness == "codex" {
        c["todo"] = json!("native");
        c["diff"] = json!("native");
    }
    if harness == "chat" {
        c["steer"] = json!("emulated");
        c["plan_mode"] = json!("unsupported");
        c["commands_list"] = json!("unsupported");
        c["durable_pause"] = json!("native");
        if protocol != "responses" {
            c["compact"] = json!("unsupported");
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
        c.llm.responses_model = "gpt-5.4".into();
        c.work.model = "gpt-5.4".into();
        c.work.claude_model = "claude-sonnet-4-6".into();
        c.llm.models = vec![
            ModelRoute {
                id: "gpt-5.2".into(),
                protocol: "responses".into(),
                harnesses: vec!["codex".into(), "chat".into()],
            },
            ModelRoute {
                id: "claude-opus-4-6".into(),
                protocol: "anthropic".into(),
                harnesses: vec!["claude".into(), "chat".into()],
            },
        ];
        c
    }

    #[test]
    fn catalog_merges_routes_and_defaults() {
        let c = catalog(&config());
        let gpt = c.iter().find(|m| m.id == "gpt-5.4").unwrap();
        assert_eq!(gpt.protocol, "responses");
        assert_eq!(gpt.harnesses, ["chat", "codex"]);
        assert_eq!(c.iter().filter(|m| m.id == "gpt-5.4").count(), 1);
        assert!(
            c.iter()
                .any(|m| m.id == "fixture-model" && m.harnesses == ["chat"])
        );
        let mut mock = config();
        mock.llm.enabled = false;
        assert_eq!(catalog(&mock)[0].id, "mock");
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
        // Explicit protocols from older clients are kept as sent.
        assert_eq!(chat(json!({"protocol":"responses"})).model, "gpt-5.4");
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
        // Older clients can still request the separate Responses default explicitly.
        assert_eq!(
            RunConfig::resolve(json!({"protocol":"responses"}), "chat", &c)
                .unwrap()
                .model,
            "gpt-5.4"
        );
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
