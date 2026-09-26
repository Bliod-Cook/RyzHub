//! Validated per-conversation settings. Each run receives an immutable snapshot.
use crate::{config::Config, store::Tx};
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

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
            protocol: "anthropic".into(),
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
        if c.harness.is_empty() {
            c.harness = if mode == "work" { "codex" } else { "chat" }.into();
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
        ensure!(
            matches!(
                (mode, c.harness.as_str()),
                ("chat", "chat") | ("work", "codex" | "claude")
            ),
            "INVALID_HARNESS"
        );
        ensure!(
            matches!(c.protocol.as_str(), "anthropic" | "responses"),
            "INVALID_PROTOCOL"
        );
        if c.harness == "codex" {
            c.protocol = "responses".into();
        }
        if c.harness == "claude" {
            c.protocol = "anthropic".into();
        }
        ensure!(
            matches!(c.approval.as_str(), "ask" | "allow_edits" | "full_auto"),
            "INVALID_APPROVAL_MODE"
        );
        ensure!(
            matches!(
                c.reasoning.as_str(),
                "low" | "medium" | "high" | "xhigh" | "max"
            ),
            "INVALID_REASONING"
        );
        ensure!(!c.plan || mode == "work", "PLAN_UNSUPPORTED");
        ensure!(
            !(c.harness == "codex" && c.reasoning == "max"),
            "INVALID_REASONING"
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
            "POLICY_DENIED"
        );
        ensure!(
            self.approval != "full_auto" || p.allow_full_auto != Some(false),
            "POLICY_DENIED"
        );
        // Catalog entries are explicit routes. Legacy default models remain routable.
        let routes = models(config, &self.harness, &self.protocol);
        ensure!(routes.contains(&self.model), "MODEL_NOT_ROUTABLE");
        Ok(())
    }
}
pub fn models(config: &Config, harness: &str, protocol: &str) -> Vec<String> {
    if !config.llm.enabled {
        return vec!["mock".into()];
    }
    let mut models: Vec<String> = config
        .llm
        .models
        .iter()
        .filter(|m| m.protocol == protocol && m.harnesses.iter().any(|h| h == harness))
        .map(|m| m.id.clone())
        .collect();
    let default = match harness {
        "codex" => &config.work.model,
        "claude" => &config.work.claude_model,
        _ if protocol == "responses" => &config.llm.responses_model,
        _ => &config.llm.model,
    };
    if !default.is_empty() && !models.contains(default) {
        models.push(default.clone());
    }
    models
}
pub fn capabilities(harness: &str, protocol: &str) -> Value {
    let mut c = json!({"steer":"native","interrupt":"native","approval_granularity":"native","ask_user":"native","resume":"native","fork":"native","compact":"native","plan_mode":"native","model_switch":"native","thought_level":"native","commands_list":"native","todo":"unsupported","diff":"unsupported","terminal_stream":"unsupported","subagent":"unsupported","durable_pause":"emulated","side_query":"emulated"});
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
