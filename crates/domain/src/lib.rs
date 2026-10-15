//! Shared authorization and policy logic. Entry points must obtain membership afresh.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Admin,
    Member,
}

#[derive(Clone, Debug)]
pub struct OrgContext {
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub role: Role,
}

#[derive(Clone, Debug)]
pub enum Principal {
    User(OrgContext),
    // TODO(org): never constructed; gateway keys are checked as their user.
    VirtualKey {
        org_id: Uuid,
        user_id: Uuid,
        resource_id: Uuid,
        revoked: bool,
    },
    // TODO(org): never constructed.
    Service {
        org_id: Uuid,
        actions: BTreeSet<Action>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    UseConversation,
    Subscribe,
    UseModel,
    UsePlugin,             // TODO(org): never checked.
    InstallPersonalPlugin, // TODO(org): never checked.
    SetPersonalCredential,
    ManageOrgPlugin, // TODO(org): never checked.
    SetOrgCredential,
    ManageMember,
    ViewOrgUsage,
    ViewAudit,
    SetPolicy,          // TODO(org): never checked.
    DeleteOrganization, // TODO(org): never checked.
}

#[derive(Clone, Debug)]
pub struct Resource {
    pub org_id: Uuid,
    pub id: Uuid,
    /// Private resources remain private even to organization administrators.
    pub owner_user_id: Option<Uuid>,
    pub target_role: Option<Role>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("access denied")]
pub struct Denied;

pub fn authorize(principal: &Principal, action: Action, resource: &Resource) -> Result<(), Denied> {
    match principal {
        Principal::User(ctx) => {
            if ctx.org_id != resource.org_id
                || resource.owner_user_id.is_some_and(|id| id != ctx.user_id)
            {
                return Err(Denied);
            }
            let allowed = match action {
                Action::UseConversation
                | Action::Subscribe
                | Action::UseModel
                | Action::UsePlugin
                | Action::InstallPersonalPlugin
                | Action::SetPersonalCredential => true,
                Action::SetPolicy | Action::DeleteOrganization => ctx.role == Role::Owner,
                Action::ManageMember => {
                    ctx.role == Role::Owner
                        || (ctx.role == Role::Admin && resource.target_role != Some(Role::Owner))
                }
                _ => matches!(ctx.role, Role::Owner | Role::Admin),
            };
            if allowed { Ok(()) } else { Err(Denied) }
        }
        Principal::VirtualKey {
            org_id,
            user_id,
            resource_id,
            revoked,
        } => {
            if !revoked
                && *org_id == resource.org_id
                && *resource_id == resource.id
                && resource.owner_user_id.is_none_or(|id| id == *user_id)
                && matches!(action, Action::UseModel | Action::UsePlugin)
            {
                Ok(())
            } else {
                Err(Denied)
            }
        }
        Principal::Service { org_id, actions } => {
            if *org_id == resource.org_id && actions.contains(&action) {
                Ok(())
            } else {
                Err(Denied)
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// None is unrestricted; Some(empty) denies everything.
    pub models: Option<BTreeSet<String>>,
    pub harnesses: Option<BTreeSet<String>>,
    pub plugin_sources: Option<BTreeSet<String>>,
    /// Egress rules sandboxes may use, within the instance's `sandbox.egress_allow`.
    pub egress_hosts: Option<BTreeSet<String>>,
    pub max_sandboxes: Option<u32>,
    /// Largest writable layer of a sandbox, in bytes.
    pub sandbox_disk_bytes: Option<u64>,
    pub compute_idle_ttl_secs: Option<u64>,
    pub auto_archive_after_secs: Option<u64>,
    pub archive_retention_secs: Option<u64>,
    pub allow_personal_credentials: Option<bool>,
    pub allow_platform_credentials: Option<bool>,
    pub allow_personal_plugins: Option<bool>,
    pub allow_memory: Option<bool>,
    pub allow_mcp_apps: Option<bool>,
    pub allow_full_auto: Option<bool>,
    pub allow_projects: Option<bool>,
    pub project_file_bytes: Option<u64>,
    /// Largest file a member may upload to their file library.
    pub attachment_bytes: Option<u64>,
    /// Total size of one member's file library.
    pub attachment_quota_bytes: Option<u64>,
    /// The most reasoning a model request may ask for: `minimal`, `low`, `medium`, `high`,
    /// `xhigh` or `max`.
    pub max_reasoning: Option<String>,
    /// Plugins neither signed by a trusted publisher nor from the official catalog.
    pub allow_unverified_plugins: Option<bool>,
    /// Plugin tools (`<plugin>__<tool>`, `<plugin>__*` for all of a plugin's) whose every Work
    /// call needs the member's approval, enforced by the gateway.
    pub approval_required_tools: Option<BTreeSet<String>>,
    /// Categories kept out of memory besides the platform's own (health, religion, …); a name
    /// the platform does not know is matched as a word.
    pub memory_forbidden_categories: Option<BTreeSet<String>>,
    /// Memory written by the platform from conversations (idle summaries), not only by the
    /// member or at their request.
    pub allow_memory_auto_extract: Option<bool>,
}

/// Reasoning levels from least to most.
pub const REASONING_LEVELS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// Whether `tool` (`<plugin>__<tool>`) matches one of `patterns` (`<plugin>__*` matches all of
/// a plugin's tools).
pub fn tool_matches(patterns: &BTreeSet<String>, tool: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| match pattern.strip_suffix('*') {
            Some(prefix) => tool.starts_with(prefix),
            None => pattern == tool,
        })
}

/// A reasoning level's rank; unknown levels rank as the most.
pub fn reasoning_rank(level: &str) -> usize {
    REASONING_LEVELS
        .iter()
        .position(|l| *l == level)
        .unwrap_or(REASONING_LEVELS.len())
}

fn least_reasoning(a: &Option<String>, b: &Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) if reasoning_rank(b) < reasoning_rank(a) => Some(b.clone()),
        (Some(a), _) => Some(a.clone()),
        (None, b) => b.clone(),
    }
}

fn union(a: &Option<BTreeSet<String>>, b: &Option<BTreeSet<String>>) -> Option<BTreeSet<String>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.union(b).cloned().collect()),
        (a, b) => a.clone().or_else(|| b.clone()),
    }
}

fn intersection(
    a: &Option<BTreeSet<String>>,
    b: &Option<BTreeSet<String>>,
) -> Option<BTreeSet<String>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.intersection(b).cloned().collect()),
        (Some(a), None) | (None, Some(a)) => Some(a.clone()),
        _ => None,
    }
}

fn cap<T: Ord + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl Policy {
    pub fn restrict(&self, lower: &Self) -> Self {
        Self {
            models: intersection(&self.models, &lower.models),
            harnesses: intersection(&self.harnesses, &lower.harnesses),
            plugin_sources: intersection(&self.plugin_sources, &lower.plugin_sources),
            egress_hosts: intersection(&self.egress_hosts, &lower.egress_hosts),
            max_sandboxes: cap(self.max_sandboxes, lower.max_sandboxes),
            sandbox_disk_bytes: cap(self.sandbox_disk_bytes, lower.sandbox_disk_bytes),
            compute_idle_ttl_secs: cap(self.compute_idle_ttl_secs, lower.compute_idle_ttl_secs),
            auto_archive_after_secs: cap(
                self.auto_archive_after_secs,
                lower.auto_archive_after_secs,
            ),
            archive_retention_secs: cap(self.archive_retention_secs, lower.archive_retention_secs),
            allow_personal_credentials: cap(
                self.allow_personal_credentials,
                lower.allow_personal_credentials,
            ),
            allow_platform_credentials: cap(
                self.allow_platform_credentials,
                lower.allow_platform_credentials,
            ),
            allow_personal_plugins: cap(self.allow_personal_plugins, lower.allow_personal_plugins),
            allow_memory: cap(self.allow_memory, lower.allow_memory),
            allow_mcp_apps: cap(self.allow_mcp_apps, lower.allow_mcp_apps),
            allow_full_auto: cap(self.allow_full_auto, lower.allow_full_auto),
            allow_projects: cap(self.allow_projects, lower.allow_projects),
            project_file_bytes: cap(self.project_file_bytes, lower.project_file_bytes),
            attachment_bytes: cap(self.attachment_bytes, lower.attachment_bytes),
            attachment_quota_bytes: cap(self.attachment_quota_bytes, lower.attachment_quota_bytes),
            max_reasoning: least_reasoning(&self.max_reasoning, &lower.max_reasoning),
            allow_unverified_plugins: cap(
                self.allow_unverified_plugins,
                lower.allow_unverified_plugins,
            ),
            // Narrowing adds tools that need approval, and categories kept out of memory.
            approval_required_tools: union(
                &self.approval_required_tools,
                &lower.approval_required_tools,
            ),
            memory_forbidden_categories: union(
                &self.memory_forbidden_categories,
                &lower.memory_forbidden_categories,
            ),
            allow_memory_auto_extract: cap(
                self.allow_memory_auto_extract,
                lower.allow_memory_auto_extract,
            ),
        }
    }

    pub fn effective(instance: &Self, org: &Self, user: &Self) -> Self {
        instance.restrict(org).restrict(user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roles_do_not_override_tenant_or_private_ownership() {
        let org = Uuid::now_v7();
        let user = Uuid::now_v7();
        let principal = Principal::User(OrgContext {
            org_id: org,
            user_id: user,
            role: Role::Owner,
        });
        let mut resource = Resource {
            org_id: org,
            id: Uuid::now_v7(),
            owner_user_id: Some(user),
            target_role: None,
        };
        assert!(authorize(&principal, Action::Subscribe, &resource).is_ok());
        resource.org_id = Uuid::now_v7();
        assert!(authorize(&principal, Action::Subscribe, &resource).is_err());
        resource.org_id = org;
        resource.owner_user_id = Some(Uuid::now_v7());
        assert!(authorize(&principal, Action::UseConversation, &resource).is_err());
    }

    #[test]
    fn lower_policy_cannot_broaden_upper_policy() {
        let instance = Policy {
            models: Some(BTreeSet::from(["a".into(), "b".into()])),
            max_sandboxes: Some(4),
            allow_memory: Some(false),
            attachment_bytes: Some(50),
            ..Default::default()
        };
        let org = Policy {
            models: Some(BTreeSet::from(["b".into(), "c".into()])),
            max_sandboxes: Some(2),
            attachment_bytes: Some(80),
            attachment_quota_bytes: Some(1000),
            ..Default::default()
        };
        let user = Policy {
            max_sandboxes: Some(99),
            allow_memory: Some(true),
            ..Default::default()
        };
        let merged = Policy::effective(&instance, &org, &user);
        assert_eq!(merged.models, Some(BTreeSet::from(["b".into()])));
        assert_eq!(merged.max_sandboxes, Some(2));
        assert_eq!(merged.allow_memory, Some(false));
        assert_eq!(merged.attachment_bytes, Some(50));
        assert_eq!(merged.attachment_quota_bytes, Some(1000));
        assert_eq!(
            merged
                .restrict(&Policy {
                    models: Some(BTreeSet::new()),
                    ..Default::default()
                })
                .models,
            Some(BTreeSet::new())
        );
    }

    #[test]
    fn admin_cannot_modify_owner_and_key_cannot_subscribe() {
        let org = Uuid::now_v7();
        let user = Uuid::now_v7();
        let id = Uuid::now_v7();
        let resource = Resource {
            org_id: org,
            id,
            owner_user_id: None,
            target_role: Some(Role::Owner),
        };
        let admin = Principal::User(OrgContext {
            org_id: org,
            user_id: user,
            role: Role::Admin,
        });
        assert!(authorize(&admin, Action::ManageMember, &resource).is_err());
        let key = Principal::VirtualKey {
            org_id: org,
            user_id: user,
            resource_id: id,
            revoked: false,
        };
        assert!(authorize(&key, Action::UseModel, &resource).is_ok());
        assert!(authorize(&key, Action::Subscribe, &resource).is_err());
    }
}
