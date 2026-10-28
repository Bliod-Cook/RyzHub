//! Run lifecycle shared by every engine: admission, inputs, pending actions and termination.
//!
//! All functions take the caller's transaction; the channel lease is locked by the caller (or
//! by [`guard`]) before any projection is written.
use anyhow::{Context, ensure};
use ryzhub_domain::OrgContext;
use ryzhub_proto::{
    AttachmentsDetail, CommandEnvelope, Item, ItemDetail, ReadState, RunState,
    event_envelope::Event, item_detail,
};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    config::Config,
    error::Code,
    run_config::RunConfig,
    store::{
        ActionRow, Lease, Store, Tx, append, effective_policy, load_item, lock_lease, upsert_item,
    },
};

/// Longest accepted user message, steer or answer.
const MAX_INPUT: usize = 65_536;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    Main,
    /// Read-only side question: no tools, never written back to the main line.
    SideQuery,
    /// Provider-native context compaction.
    Compact,
}

pub fn valid_text(text: &str) -> anyhow::Result<()> {
    ensure!(
        !text.trim().is_empty() && text.len() <= MAX_INPUT,
        Code::InvalidMessage
    );
    Ok(())
}

/// What a member sent: text and files of their own library. With files, the text may be empty.
#[derive(Clone, Copy)]
pub struct Input<'a> {
    pub text: &'a str,
    /// Library entry ids, in order.
    pub files: &'a [String],
    /// The sender, who must own every file.
    pub user: Uuid,
}

impl<'a> Input<'a> {
    pub fn text(text: &'a str) -> Self {
        Self {
            text,
            files: &[],
            user: Uuid::nil(),
        }
    }
}

pub fn valid_input(input: &Input) -> anyhow::Result<()> {
    if input.files.is_empty() {
        return valid_text(input.text);
    }
    ensure!(input.text.len() <= MAX_INPUT, Code::InvalidMessage);
    ensure!(
        input.files.len() <= crate::attachments::MAX_PER_MESSAGE,
        Code::TooManyAttachments
    );
    let distinct: std::collections::HashSet<&String> = input.files.iter().collect();
    ensure!(
        distinct.len() == input.files.len(),
        Code::AttachmentUnavailable
    );
    Ok(())
}

/// A fenced transaction for work on `run` (see [`guard`]).
pub async fn guarded<'a>(store: &'a Store, lease: &Lease, run: Uuid) -> anyhow::Result<Tx<'a>> {
    let mut tx = store.tx(lease.org_id).await?;
    guard(&mut tx, lease, run).await?;
    Ok(tx)
}

/// Lock the lease and require `run` to be running under it for a creator who is still an
/// active member.
pub async fn guard(tx: &mut Tx<'_>, lease: &Lease, run: Uuid) -> anyhow::Result<()> {
    lock_lease(tx, lease).await?;
    let valid = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM runs r
             JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id
             JOIN memberships m ON m.org_id=c.org_id AND m.user_id=c.creator_id
             JOIN users u ON u.id=c.creator_id AND u.status='active'
         WHERE r.org_id=$1 AND r.id=$2 AND r.exec_epoch=$3 AND r.state='running') AS "valid!""#,
        lease.org_id,
        run,
        lease.epoch
    )
    .fetch_one(&mut **tx)
    .await?;
    ensure!(valid, Code::Fenced);
    Ok(())
}

async fn announce_run(
    tx: &mut Tx<'_>,
    lease: &Lease,
    cid: &str,
    run: Uuid,
    state: &str,
    read_only: bool,
) -> anyhow::Result<()> {
    let (started_at_ms, ended_at_ms): (Option<i64>, Option<i64>) = sqlx::query_as(concat!(
        "SELECT ",
        run_times!(),
        " FROM runs WHERE org_id=$1 AND id=$2"
    ))
    .bind(lease.org_id)
    .bind(run)
    .fetch_one(&mut **tx)
    .await?;
    append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        cid,
        Event::RunStateChanged(RunState {
            read_only,
            run_id: run.to_string(),
            state: state.into(),
            exec_epoch: lease.epoch as u64,
            started_at_ms: started_at_ms.unwrap_or_default(),
            ended_at_ms: ended_at_ms.unwrap_or_default(),
        }),
    )
    .await?;
    Ok(())
}

/// Admission under the organization's `max_sandboxes`, serialized organization-wide. A
/// conversation holds execution resources while it has an active run or a live sandbox (an idle
/// Work sandbox keeps its container until suspended); the conversation being admitted does not
/// count against itself.
async fn admit(tx: &mut Tx<'_>, lease: &Lease, limit: Option<u32>) -> anyhow::Result<()> {
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(lease.org_id)
        .fetch_one(&mut **tx)
        .await?;
    if let Some(limit) = limit {
        let busy: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT conversation_id) FROM (
                 SELECT conversation_id FROM runs WHERE org_id=$1 AND state IN ('running','requires_action')
                 UNION ALL
                 SELECT conversation_id FROM sandboxes WHERE org_id=$1 AND state IN ('provisioning','running')
             ) busy WHERE conversation_id<>$2",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .fetch_one(&mut **tx)
        .await?;
        ensure!(busy < limit as i64, Code::OrgConcurrencyLimit);
    }
    Ok(())
}

async fn main_run_busy(tx: &mut Tx<'_>, lease: &Lease) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM runs WHERE org_id=$1 AND conversation_id=$2 AND NOT read_only
         AND state IN ('running','requires_action'))",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut **tx)
    .await?)
}

/// Start a run for a user message. The run's configuration, model context seed and project
/// content are frozen now; later edits only affect later runs.
pub async fn create(
    tx: &mut Tx<'_>,
    lease: &Lease,
    cmd: &CommandEnvelope,
    input: Input<'_>,
    kind: RunKind,
    config: &Config,
) -> anyhow::Result<Uuid> {
    ensure!(config.llm.enabled, Code::ModelGatewayDisabled);
    // Files reach models and sandboxes of the main line only.
    ensure!(
        input.files.is_empty() || kind == RunKind::Main,
        Code::UnsupportedCommand
    );
    valid_input(&input)?;
    let read_only = kind == RunKind::SideQuery;
    let conversation = sqlx::query(
        "SELECT mode,run_config,context_seed,project_id FROM conversations
         WHERE org_id=$1 AND id=$2",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut **tx)
    .await?;
    let mode: String = conversation.get("mode");
    let work = mode == "work";
    let operation_busy: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM conversation_operations WHERE org_id=$1 AND conversation_id=$2
         AND state IN ('queued','running'))",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_one(&mut **tx)
    .await?;
    ensure!(!operation_busy, Code::ConversationBusy);
    if work {
        ensure!(config.work.enabled, Code::WorkDisabled);
        crate::work::available(tx, lease).await?;
        let workspace_busy: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND conversation_id=$2
             AND kind IN ('archive','restore') AND state IN ('queued','running'))",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .fetch_one(&mut **tx)
        .await?;
        ensure!(!workspace_busy, Code::WorkspaceBusy);
    }
    let mut settings = RunConfig::resolve(conversation.get("run_config"), &mode, config)?;
    let policy = if work && read_only {
        // A Work side question is answered by the Chat loop without touching the sandbox.
        settings.harness = "chat".into();
        let policy = effective_policy(tx, &config.policy, lease.org_id).await?;
        ensure!(settings.allowed(&policy), Code::PolicyDenied);
        policy
    } else {
        settings.validate(tx, lease.org_id, config).await?
    };
    admit(tx, lease, policy.max_sandboxes).await?;
    if !read_only {
        ensure!(!main_run_busy(tx, lease).await?, Code::RunBusyUseSteer);
    }
    let seed = context_seed(
        tx,
        lease,
        &settings,
        kind,
        work,
        conversation.get("context_seed"),
    )
    .await?;
    let parent: Option<Uuid> = if read_only {
        sqlx::query_scalar(
            "SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2 AND NOT read_only
             ORDER BY started_at DESC LIMIT 1",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .fetch_optional(&mut **tx)
        .await?
    } else {
        None
    };
    let project: Option<Uuid> = conversation.get("project_id");
    let revision = match project {
        Some(project) => crate::projects::snapshot(tx, lease.org_id, project).await?,
        None => None,
    };
    let run = Uuid::now_v7();
    let item = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO runs(id,org_id,conversation_id,harness,model,protocol,run_config,exec_epoch,state,
             response_text,assistant_item_id,cause_cid,read_only,parent_run_id,seed_messages,seed_run_id,
             seed_step,project_id,project_revision_id,operation)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,'running','',$9,$10,$11,$12,$13,$14,$15,$16,$17,$18)",
    )
    .bind(run)
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(&settings.harness)
    .bind(&settings.model)
    .bind(&settings.protocol)
    .bind(serde_json::to_value(&settings)?)
    .bind(lease.epoch)
    .bind(item)
    .bind(Uuid::parse_str(&cmd.cid)?)
    .bind(read_only)
    .bind(parent)
    .bind(seed.messages)
    .bind(seed.base.map(|(run, _)| run))
    .bind(seed.base.map(|(_, step)| step))
    .bind(project)
    .bind(revision)
    .bind(if kind == RunKind::Compact { "compact" } else { "" })
    .execute(&mut **tx)
    .await?;
    crate::plugins::fix_run(tx, lease.org_id, lease.conversation_id, run).await?;
    add_input(tx, lease, run, input, "message", &cmd.cid).await?;
    append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        &cmd.cid,
        Event::ItemUpserted(Item {
            id: item.to_string(),
            revision: 1,
            kind: if read_only { "side_query" } else { "assistant" }.into(),
            state: "streaming".into(),
            run_id: run.to_string(),
            ..Default::default()
        }),
    )
    .await?;
    announce_run(tx, lease, &cmd.cid, run, "running", read_only).await?;
    Ok(run)
}

/// Where a new run's model context starts.
struct Seed {
    /// Explicit messages: first run, fork seed or side-question snapshot.
    messages: Value,
    /// Or the completed step whose context this run continues.
    base: Option<(Uuid, i32)>,
}

async fn context_seed(
    tx: &mut Tx<'_>,
    lease: &Lease,
    settings: &RunConfig,
    kind: RunKind,
    work: bool,
    fork_seed: Option<Value>,
) -> anyhow::Result<Seed> {
    if work && kind == RunKind::SideQuery {
        let texts: Vec<String> = sqlx::query_scalar(
            "SELECT text FROM items WHERE org_id=$1 AND conversation_id=$2 AND kind IN ('user','assistant')
             AND state='completed' ORDER BY first_seq DESC LIMIT 20",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .fetch_all(&mut **tx)
        .await?;
        let summary: String = texts.into_iter().rev().collect::<Vec<_>>().join("\n");
        let summary: String = summary.chars().take(24_000).collect();
        return Ok(Seed {
            messages: json!([{"role":"user","content":format!(
                "Read-only side question. Main conversation summary (no filesystem access):\n{summary}"
            )}]),
            base: None,
        });
    }
    // The newest step on the main line with this wire protocol. A side question may read the
    // active run; a new main run continues only from runs that ended normally.
    let latest: Option<(Uuid, i32)> = sqlx::query_as(
        "SELECT r.id,s.step_no FROM loop_steps s JOIN runs r ON r.org_id=s.org_id AND r.id=s.run_id
         WHERE s.org_id=$1 AND r.conversation_id=$2 AND r.protocol=$3 AND NOT r.read_only
           AND r.state IN ('running','requires_action','completed')
         ORDER BY r.started_at DESC,s.step_no DESC LIMIT 1",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(&settings.protocol)
    .fetch_optional(&mut **tx)
    .await?;
    let initial = fork_seed.unwrap_or_else(|| json!([]));
    match (latest, kind) {
        (None, _) => Ok(Seed {
            messages: initial,
            base: None,
        }),
        // A side question gets its own snapshot of the committed prefix, never a pointer into
        // a run that is still changing.
        (Some((run, step)), RunKind::SideQuery) => Ok(Seed {
            messages: crate::chat::context::committed_prefix(tx, lease.org_id, run, step).await?,
            base: None,
        }),
        (Some(base), _) => Ok(Seed {
            messages: json!([]),
            base: Some(base),
        }),
    }
}

/// Record a user message or steer: a timeline item plus a pending input for the engine. Its
/// files become references of this conversation.
pub async fn add_input(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    input: Input<'_>,
    kind: &str,
    cid: &str,
) -> anyhow::Result<()> {
    let item = Uuid::now_v7();
    let steer = kind == "steer";
    let files = crate::attachments::bind(tx, lease, input.user, input.files).await?;
    let ids: Vec<Uuid> = files.iter().map(|(id, _)| *id).collect();
    let detail = (!files.is_empty()).then(|| ItemDetail {
        kind: Some(item_detail::Kind::Attachments(AttachmentsDetail {
            files: files.into_iter().map(|(_, file)| file).collect(),
        })),
    });
    append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        cid,
        Event::ItemUpserted(Item {
            id: item.to_string(),
            revision: 1,
            kind: if steer { "steer" } else { "user" }.into(),
            state: if steer { "accepted" } else { "completed" }.into(),
            text: input.text.into(),
            run_id: run.to_string(),
            detail,
            ..Default::default()
        }),
    )
    .await?;
    sqlx::query(
        "INSERT INTO pending_inputs(org_id,run_id,text,kind,item_id,attachment_ids)
         VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(lease.org_id)
    .bind(run)
    .bind(input.text)
    .bind(kind)
    .bind(item)
    .bind(&ids)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Additional input for the active main run.
pub async fn steer(
    tx: &mut Tx<'_>,
    lease: &Lease,
    input: Input<'_>,
    cid: &str,
) -> anyhow::Result<()> {
    valid_input(&input)?;
    let run: Uuid = sqlx::query_scalar(
        "SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2 AND NOT read_only
         AND state IN ('running','requires_action')",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_optional(&mut **tx)
    .await?
    .context(Code::NoActiveRun)?;
    add_input(tx, lease, run, input, "steer", cid).await
}

/// Insert a pending approval or question for `run`, expiring after the configured TTL.
pub async fn new_action(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    kind: &str,
    summary: &str,
    options: &[&str],
    config: &Config,
) -> anyhow::Result<ActionRow> {
    Ok(sqlx::query_as(concat!(
        "INSERT INTO pending_actions(id,org_id,run_id,run_epoch,kind,summary,options,expires_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,now()+($8::bigint * interval '1 second')) RETURNING ",
        action_columns!()
    ))
    .bind(Uuid::now_v7())
    .bind(lease.org_id)
    .bind(run)
    .bind(lease.epoch)
    .bind(kind)
    .bind(summary)
    .bind(options)
    .bind(config.approval_ttl_secs as i64)
    .fetch_one(&mut **tx)
    .await?)
}

/// Move `run` to requires_action and announce the new pending action.
pub async fn request_action(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    action: ActionRow,
) -> anyhow::Result<()> {
    crate::push::notify(
        tx,
        lease.org_id,
        lease.conversation_id,
        crate::push::Notice::Action {
            id: action.id,
            kind: &action.kind,
            summary: &action.summary,
        },
    )
    .await?;
    append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        "",
        Event::PendingActionChanged(action.into()),
    )
    .await?;
    sqlx::query("UPDATE runs SET state='requires_action' WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(run)
        .execute(&mut **tx)
        .await?;
    announce_run(tx, lease, "", run, "requires_action", false).await
}

pub struct Resolution<'a> {
    pub user: Uuid,
    pub device: &'a str,
    pub action_id: &'a str,
    pub run_epoch: u64,
    /// `approval` or `question`.
    pub kind: &'a str,
    /// allow/deny for approvals, the answer text for questions.
    pub answer: &'a str,
    pub cid: &'a str,
}

/// Resolve a pending action with a compare-and-set on its status, epoch and expiry. The run
/// returns to `running` once no other action of it is pending. Returns the run.
pub async fn resolve(tx: &mut Tx<'_>, lease: &Lease, r: Resolution<'_>) -> anyhow::Result<Uuid> {
    let id = Uuid::parse_str(r.action_id).context(Code::InvalidAction)?;
    let status = if r.kind == "question" {
        "answered"
    } else {
        r.answer
    };
    let action: ActionRow = sqlx::query_as(
        "UPDATE pending_actions a SET status=$5,resolved_by=$6,resolved_device=$7
         FROM runs r
         WHERE a.org_id=$1 AND a.id=$2 AND a.run_epoch=$3 AND a.status='pending' AND a.kind=$8
           AND a.expires_at>now() AND r.org_id=a.org_id AND r.id=a.run_id AND r.conversation_id=$4
           AND r.exec_epoch=$3 AND r.state='requires_action'
         RETURNING a.id,a.run_id,a.run_epoch,a.kind,a.summary,a.options,a.status,a.resolved_device,a.expires_at,a.form",
    )
    .bind(lease.org_id)
    .bind(id)
    .bind(r.run_epoch as i64)
    .bind(lease.conversation_id)
    .bind(status)
    .bind(r.user)
    .bind(r.device)
    .bind(r.kind)
    .fetch_optional(&mut **tx)
    .await?
    .context(Code::ActionAlreadyResolved)?;
    let run = action.run_id;
    let native: Option<Value> =
        sqlx::query_scalar("SELECT params FROM native_requests WHERE org_id=$1 AND action_id=$2")
            .bind(lease.org_id)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(request) = native {
        let answer = crate::work::answer(&request, r.kind, r.answer)?;
        // The member allowed a call of a gateway tool: the gateway need not ask again.
        if r.answer == "allow"
            && let Some(tool) = request["tool"]
                .as_str()
                .and_then(|tool| tool.strip_prefix("mcp__ryzhub__"))
        {
            sqlx::query(
                "INSERT INTO approval_grants(id,org_id,run_id,exec_epoch,tool,args_digest,expires_at)
                 VALUES($1,$2,$3,$4,$5,$6,now()+interval '1 hour')",
            )
            .bind(Uuid::now_v7())
            .bind(lease.org_id)
            .bind(action.run_id)
            .bind(action.run_epoch)
            .bind(tool)
            .bind(crate::mcp::digest(tool, &request["arguments"]))
            .execute(&mut **tx)
            .await?;
        }
        sqlx::query("UPDATE native_requests SET answer=$3 WHERE org_id=$1 AND action_id=$2")
            .bind(lease.org_id)
            .bind(id)
            .bind(serde_json::to_value(answer)?)
            .execute(&mut **tx)
            .await?;
    } else if r.kind == "question" {
        crate::chat::tools::answer_question(tx, lease, id, r.answer).await?;
    }
    append(
        tx,
        lease.org_id,
        &lease.channel(),
        Some(lease),
        r.cid,
        Event::PendingActionChanged(action.into()),
    )
    .await?;
    let state: String = sqlx::query_scalar(
        "UPDATE runs r SET state=CASE WHEN EXISTS(SELECT 1 FROM pending_actions a
             WHERE a.org_id=r.org_id AND a.run_id=r.id AND a.status='pending')
             THEN 'requires_action' ELSE 'running' END
         WHERE r.org_id=$1 AND r.id=$2 RETURNING state",
    )
    .bind(lease.org_id)
    .bind(run)
    .fetch_one(&mut **tx)
    .await?;
    announce_run(tx, lease, r.cid, run, &state, false).await?;
    Ok(run)
}

/// Terminate an active run: finalize its visible item, drop unconsumed steers, invalidate its
/// pending actions and revoke its gateway keys. A no-op for runs that already ended.
pub async fn finish(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    state: &str,
    error: Option<Code>,
) -> anyhow::Result<()> {
    let row = sqlx::query(
        "UPDATE runs SET state=$4,ended_at=now(),error_code=$5
         WHERE org_id=$1 AND id=$2 AND exec_epoch=$3 AND state IN ('running','requires_action')
         RETURNING assistant_item_id,response_text,read_only",
    )
    .bind(lease.org_id)
    .bind(run)
    .bind(lease.epoch)
    .bind(state)
    .bind(error.map(|code| code.as_str()))
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let steers: Vec<Uuid> = sqlx::query_scalar(
        "SELECT item_id FROM pending_inputs WHERE org_id=$1 AND run_id=$2 AND kind='steer' AND consumed_step IS NULL",
    )
    .bind(lease.org_id)
    .bind(run)
    .fetch_all(&mut **tx)
    .await?;
    for id in steers {
        let mut item = load_item(tx, lease.org_id, id).await?;
        item.revision += 1;
        item.state = "cancelled".into();
        upsert_item(tx, lease, item).await?;
    }
    let mut item = load_item(tx, lease.org_id, row.get("assistant_item_id")).await?;
    if item.state == "streaming" || state != "completed" {
        item.revision += 1;
        item.state = state.into();
        item.text = row.get("response_text");
        if let (Some(code), "failed") = (error, state) {
            item.text.push_str(&format!("\n\n执行失败：{code}"));
        }
        upsert_item(tx, lease, item).await?;
    }
    let actions: Vec<ActionRow> = sqlx::query_as(concat!(
        "UPDATE pending_actions SET status='invalidated' WHERE org_id=$1 AND run_id=$2 AND status='pending' RETURNING ",
        action_columns!()
    ))
    .bind(lease.org_id)
    .bind(run)
    .fetch_all(&mut **tx)
    .await?;
    for action in actions {
        append(
            tx,
            lease.org_id,
            &lease.channel(),
            Some(lease),
            "",
            Event::PendingActionChanged(action.into()),
        )
        .await?;
    }
    sqlx::query(
        "UPDATE virtual_keys SET revoked_at=now() WHERE org_id=$1 AND run_id=$2 AND revoked_at IS NULL",
    )
    .bind(lease.org_id)
    .bind(run)
    .execute(&mut **tx)
    .await?;
    let read_only: bool = row.get("read_only");
    announce_run(tx, lease, "", run, state, read_only).await?;
    if !read_only && matches!(state, "completed" | "failed") {
        let ended = crate::push::Notice::Ended { run, state };
        crate::push::notify(tx, lease.org_id, lease.conversation_id, ended).await?;
    }
    // A main run's end is news to the user's other views until they read it.
    if !read_only {
        let head = crate::store::head_seq(tx, lease.org_id, &lease.channel()).await?;
        sqlx::query("UPDATE conversations SET activity_seq=$3 WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .bind(head as i64)
            .execute(&mut **tx)
            .await?;
        crate::store::publish_conversation(tx, lease.org_id, lease.conversation_id).await?;
    }
    Ok(())
}

/// Stop every active run of the conversation.
pub async fn cancel_all(tx: &mut Tx<'_>, lease: &Lease) -> anyhow::Result<()> {
    let runs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM runs WHERE org_id=$1 AND conversation_id=$2 AND state IN ('running','requires_action')",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_all(&mut **tx)
    .await?;
    for run in runs {
        finish(tx, lease, run, "cancelled", None).await?;
    }
    Ok(())
}

/// Advance the user's read position (never backwards) and broadcast it to their devices.
pub async fn mark_read(
    tx: &mut Tx<'_>,
    lease: &Lease,
    ctx: &OrgContext,
    read_seq: u64,
    cid: &str,
) -> anyhow::Result<()> {
    let head = crate::store::head_seq(tx, lease.org_id, &lease.channel()).await?;
    ensure!(read_seq <= head, Code::InvalidReadSeq);
    let seq: i64 = sqlx::query_scalar(
        "INSERT INTO read_states(org_id,user_id,conversation_id,read_seq) VALUES($1,$2,$3,$4)
         ON CONFLICT(user_id,conversation_id) DO UPDATE SET read_seq=greatest(read_states.read_seq,EXCLUDED.read_seq)
         RETURNING read_seq",
    )
    .bind(lease.org_id)
    .bind(ctx.user_id)
    .bind(lease.conversation_id)
    .bind(read_seq as i64)
    .fetch_one(&mut **tx)
    .await?;
    append(
        tx,
        lease.org_id,
        &format!("user:{}:{}", lease.org_id, ctx.user_id),
        None,
        cid,
        Event::ReadStateChanged(ReadState {
            user_id: ctx.user_id.to_string(),
            conversation_id: lease.conversation_id.to_string(),
            read_seq: seq as u64,
        }),
    )
    .await?;
    Ok(())
}

/// Runs whose creator lost access (left, or was disabled) or whose pending action expired end
/// as cancelled.
pub async fn expire_invalid(tx: &mut Tx<'_>, lease: &Lease) -> anyhow::Result<()> {
    let invalid: Vec<Uuid> = sqlx::query_scalar(
        "SELECT r.id FROM runs r JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id
         WHERE r.org_id=$1 AND r.conversation_id=$2 AND r.state IN ('running','requires_action')
           AND (NOT EXISTS(SELECT 1 FROM memberships m JOIN users u ON u.id=m.user_id
                           WHERE m.org_id=c.org_id AND m.user_id=c.creator_id AND u.status='active')
                OR EXISTS(SELECT 1 FROM pending_actions a WHERE a.org_id=r.org_id AND a.run_id=r.id
                          AND a.status='pending' AND a.expires_at<=now()))",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .fetch_all(&mut **tx)
    .await?;
    for run in invalid {
        finish(
            tx,
            lease,
            run,
            "cancelled",
            Some(Code::AuthorizationOrActionExpired),
        )
        .await?;
    }
    Ok(())
}
