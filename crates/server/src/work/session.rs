//! Provisioning the persistent Work sandbox and its native session.
use anyhow::{Context, ensure};
use chrono::{DateTime, Utc};
use ryzhub_execution::{GuestCommand, HarnessRequest, HostCommand, SandboxSpec, TurnSettings};
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

use super::{harness, run_settings};
use crate::{
    app::App,
    error::Code,
    llm::keys,
    run_config::RunConfig,
    sandbox,
    store::{Lease, Tx},
};

/// How often a run waiting for an idle suspension to finish checks again.
const SUSPENSION_WAIT: Duration = Duration::from_millis(500);

/// Refuse to start a Work run while a failed restore awaits repair: continuing would create a
/// blank workspace and silently lose the durable one.
pub async fn available(tx: &mut Tx<'_>, lease: &Lease) -> anyhow::Result<()> {
    let recovery: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspace_heads WHERE org_id=$1 AND conversation_id=$2 AND recovery_required)",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut **tx)
    .await?;
    ensure!(!recovery, Code::WorkspaceRecoveryRequired);
    Ok(())
}

pub fn turn_settings(settings: &RunConfig) -> TurnSettings {
    TurnSettings {
        model: settings.model.clone(),
        reasoning: settings.reasoning.clone(),
        approval: settings.approval.clone(),
        plan: settings.plan,
    }
}

/// Where the sandbox for this run comes from.
enum Source {
    /// The conversation's live sandbox.
    Running,
    /// A fork: the parent's checkpoint, restored into a new workspace.
    Fork { manifest: Value },
    /// An idle-suspended sandbox, resumed from the checkpoint taken when it stopped.
    Suspended {
        thread: Option<String>,
        manifest: Value,
    },
    /// The conversation's first run.
    Fresh,
}

/// The conversation's running sandbox, provisioning or resuming it first. Returns its id.
pub async fn ensure_work(app: &App, lease: &Lease, run: Uuid) -> anyhow::Result<Uuid> {
    let settings = run_settings(app, lease, run).await?;
    let cfg = &app.config.sandbox;
    // An idle suspension checkpoints and then stops the sandbox. A run admitted meanwhile must
    // not start using it: wait, then resume from the checkpoint the suspension took. No new
    // suspension can start once this run exists, and the check shares the lease lock with the
    // job runner, so nothing slips in between.
    let mut tx = loop {
        let mut tx = app.store.fenced(lease).await?;
        if !suspending(&mut tx, lease).await? {
            break tx;
        }
        tx.rollback().await?;
        tokio::time::sleep(SUSPENSION_WAIT).await;
    };
    let running: Option<(Uuid, Value, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT id,handle,node_lease_until FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent
         AND state IN ('running','provisioning') FOR UPDATE",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_optional(&mut *tx)
    .await?;
    available(&mut tx, lease).await?;
    let fork: Option<Value> =
        sqlx::query_scalar("SELECT native_fork FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut *tx)
            .await?;
    let limits = sandbox::limits(&mut tx, app, lease.org_id).await?;
    let env = sandbox::environment(app, &settings.env_profile, true)?;
    let mut spec = SandboxSpec {
        id: Uuid::now_v7(),
        org_id: lease.org_id,
        workspace_id: lease.conversation_id,
        exec_epoch: lease.epoch as u64,
        image: env.image,
        runtime: cfg.runtime.clone(),
        memory_bytes: env.memory_bytes,
        pids_limit: env.pids_limit,
        persistent: true,
        egress: limits.egress,
        disk_bytes: env
            .disk_cap
            .map_or(limits.disk_bytes, |cap| cap.min(limits.disk_bytes)),
        deps: env.deps,
    };
    let source = match (running, fork) {
        (Some((id, handle, node_lease)), _) => {
            ensure!(
                node_lease.is_none_or(|until| until > Utc::now()),
                Code::WorkspaceRecoveryRequired
            );
            // A workspace keeps the image, layer and runtime it was created with.
            if let Ok(pinned) = serde_json::from_value::<SandboxSpec>(handle) {
                spec.image = pinned.image;
                spec.deps = pinned.deps;
                spec.runtime = pinned.runtime;
            }
            spec.id = id;
            Source::Running
        }
        (None, Some(fork)) => {
            spec.image = fork["manifest"]["image"]
                .as_str()
                .context("fork manifest without image")?
                .into();
            spec.deps = fork["manifest"]["deps"].as_str().map(Into::into);
            Source::Fork {
                manifest: fork["manifest"].clone(),
            }
        }
        (None, None) => match suspended(&mut tx, lease).await? {
            Some((thread, manifest)) => {
                spec.image = manifest["image"]
                    .as_str()
                    .context("checkpoint manifest without image")?
                    .into();
                spec.deps = manifest["deps"].as_str().map(Into::into);
                Source::Suspended { thread, manifest }
            }
            None => {
                let used: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM sandboxes WHERE org_id=$1 AND conversation_id=$2 AND persistent)
                     OR EXISTS(SELECT 1 FROM workspace_heads WHERE org_id=$1 AND conversation_id=$2
                               AND checkpoint_id IS NOT NULL)",
                )
                .bind(lease.org_id)
                .bind(lease.conversation_id)
                .fetch_one(&mut *tx)
                .await?;
                let purged: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM workspace_heads WHERE org_id=$1 AND conversation_id=$2
                     AND storage_state IN ('purging','deleted'))",
                )
                .bind(lease.org_id)
                .bind(lease.conversation_id)
                .fetch_one(&mut *tx)
                .await?;
                ensure!(!purged, Code::WorkspacePurged);
                ensure!(!used, Code::WorkspaceRecoveryRequired);
                Source::Fresh
            }
        },
    };
    let id = spec.id;
    if !matches!(source, Source::Running) {
        let generation: i64 = sqlx::query_scalar(
            "INSERT INTO workspace_heads(org_id,conversation_id) VALUES($1,$2)
             ON CONFLICT(org_id,conversation_id) DO UPDATE SET generation=workspace_heads.generation+1,
               storage_state='hot'
             RETURNING generation",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .fetch_one(&mut *tx)
        .await?;
        let thread = match &source {
            Source::Suspended { thread, .. } => thread.clone(),
            _ => None,
        };
        let node = sandbox::place(&mut tx, app, lease.conversation_id).await?;
        sqlx::query(
            "INSERT INTO sandboxes(id,org_id,conversation_id,run_id,lease_epoch,exec_epoch,state,handle,persistent,native_thread_id,node)
             VALUES($1,$2,$3,$4,$5,$6,'provisioning',$7,true,$8,$9)",
        )
        .bind(id)
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .bind(run)
        .bind(generation)
        .bind(lease.epoch)
        .bind(serde_json::to_value(&spec)?)
        .bind(thread)
        .bind(node)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    match &source {
        Source::Running => {}
        Source::Fork { manifest } | Source::Suspended { manifest, .. } => {
            crate::workspace::restore_into(app, lease, spec.clone(), manifest).await?
        }
        Source::Fresh => {
            sandbox::host(app, lease.org_id, id, HostCommand::Create { spec }).await?;
        }
    }
    sandbox::guest(app, lease, id, GuestCommand::Fence).await?;
    {
        let mut tx = app.store.fenced(lease).await?;
        sqlx::query(
            "UPDATE sandboxes SET state='running',exec_epoch=$3,run_id=$4,node_lease_until=now()+interval '15 seconds'
             WHERE org_id=$1 AND id=$2",
        )
        .bind(lease.org_id)
        .bind(id)
        .bind(lease.epoch)
        .bind(run)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE runs SET sandbox_id=$3 WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(run)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    refresh_key(app, lease, run, id).await?;
    crate::projects::mount(app, lease, run, id).await?;
    crate::attachments::sandbox::sync(app, lease, id).await?;
    // The plugins' skills, for the harness to find when it starts.
    let skills = {
        let mut tx = app.store.tx(lease.org_id).await?;
        let skills = crate::plugins::run_skills(&mut tx, lease.org_id, run).await?;
        tx.commit().await?;
        skills
    };
    sandbox::guest(
        app,
        lease,
        id,
        GuestCommand::InstallSkills {
            skills: skills
                .iter()
                .map(|(plugin, skill)| skill_files(plugin, skill))
                .collect(),
        },
    )
    .await?;
    sandbox::guest(
        app,
        lease,
        id,
        GuestCommand::NativeStart {
            harness: settings.harness.clone(),
            model: settings.model.clone(),
            mcp: app.config.search.enabled || !app.config.work.mcp_tools.is_empty(),
            acp: crate::run_config::acp(&app.config, &settings.harness).cloned(),
            models: crate::run_config::models(&app.config, &settings.harness, &settings.protocol),
        },
    )
    .await?;
    discover_models(app, lease, id, &settings).await?;
    Ok(id)
}

/// Whether an idle suspension of the conversation's workspace is queued or in progress.
async fn suspending(tx: &mut Tx<'_>, lease: &Lease) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2
         AND kind='suspend' AND state IN ('queued','running'))",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut **tx)
    .await?)
}

/// Record the harness's models with the sandbox's capabilities. Discovery runs once per
/// sandbox: the guest keeps an operation's result only for a limited time, so a fixed
/// operation id cannot be asked again forever, and the model list does not change while the
/// sandbox (and so the harness version) stays the same.
async fn discover_models(
    app: &App,
    lease: &Lease,
    id: Uuid,
    settings: &RunConfig,
) -> anyhow::Result<()> {
    let known: bool = {
        let mut tx = app.store.tx(lease.org_id).await?;
        let known = sqlx::query_scalar(
            "SELECT capabilities ? 'models' FROM sandboxes WHERE org_id=$1 AND id=$2",
        )
        .bind(lease.org_id)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        known
    };
    if known {
        return Ok(());
    }
    let listed = harness(
        app,
        lease,
        id,
        format!("{id}:models"),
        HarnessRequest::ListModels,
    )
    .await?;
    let mut capabilities = crate::run_config::capabilities(&settings.harness, &settings.protocol);
    capabilities["models"] = serde_json::to_value(listed.models)?;
    let mut tx = app.store.fenced(lease).await?;
    sqlx::query("UPDATE sandboxes SET capabilities=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(id)
        .bind(capabilities)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// The native thread and head checkpoint of a sandbox that was suspended while idle.
async fn suspended(
    tx: &mut Tx<'_>,
    lease: &Lease,
) -> anyhow::Result<Option<(Option<String>, Value)>> {
    let latest: Option<(String, Option<String>, Option<Value>)> = sqlx::query_as(
        "SELECT s.state,s.native_thread_id,c.manifest FROM sandboxes s
         LEFT JOIN workspace_heads h ON h.org_id=s.org_id AND h.conversation_id=s.conversation_id
         LEFT JOIN checkpoints c ON c.org_id=h.org_id AND c.id=h.checkpoint_id AND c.state='committed'
         WHERE s.org_id=$1 AND s.conversation_id=$2 AND s.persistent
         ORDER BY s.created_at DESC LIMIT 1",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(match latest {
        Some((state, thread, Some(manifest))) if state == "suspended" => Some((thread, manifest)),
        _ => None,
    })
}

/// Issue a fresh sandbox key for `run` and hand it to the guest. Returns its expiry.
pub async fn refresh_key(
    app: &App,
    lease: &Lease,
    run: Uuid,
    sandbox: Uuid,
) -> anyhow::Result<DateTime<Utc>> {
    let (user, credential) = sandbox::run_identity(app, lease, run).await?;
    let (generation, model): (i64, String) = {
        let mut tx = app.store.fenced(lease).await?;
        let row = sqlx::query_as(
            "SELECT s.lease_epoch,r.model FROM sandboxes s JOIN runs r ON r.org_id=s.org_id AND r.id=$3
             WHERE s.org_id=$1 AND s.id=$2 AND s.state='running'",
        )
        .bind(lease.org_id)
        .bind(sandbox)
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        row
    };
    let key = keys::issue_sandbox_key(
        &app.store,
        keys::SandboxKeyRequest {
            org_id: lease.org_id,
            user_id: user,
            conversation_id: lease.conversation_id,
            run_id: run,
            sandbox_id: sandbox,
            lease_epoch: generation,
            models: vec![model],
            credential_id: credential,
        },
    )
    .await?;
    sandbox::rotate_key(app, lease, sandbox, key.token).await?;
    Ok(key.expires_at)
}

/// A plugin skill as the harnesses read it: in a directory `<plugin>-<skill>`, which its
/// `SKILL.md` names too.
fn skill_files(
    plugin: &str,
    skill: &crate::plugins::package::Skill,
) -> ryzhub_execution::SkillFiles {
    let name: String = format!("{plugin}-{}", skill.name)
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    let (_, body) = crate::plugins::package::front_matter(&skill.body);
    let manifest = format!(
        "---\nname: {name}\ndescription: {}\n---\n{body}",
        skill.description.replace('\n', " ")
    );
    let mut files = skill.files.clone();
    files.insert("SKILL.md".into(), manifest);
    ryzhub_execution::SkillFiles { name, files }
}
