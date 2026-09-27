//! Work engine: a native harness in a persistent sandbox.
//!
//! The Owner never speaks the harness's protocol. It sends [`HarnessRequest`]s and projects
//! [`ryzhub_execution::HarnessEvent`]s; the guest translates both. The drive loop waits on a
//! guest long poll and on the Owner's command wake-up, never on a timer.
mod projection;
mod session;

pub use projection::answer;
pub use session::{available, ensure_work, refresh_key};

use anyhow::{Context, ensure};
use chrono::Utc;
use ryzhub_execution::{GuestCommand, HarnessAnswer, HarnessRequest, HarnessResult};
use serde_json::Value;
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

use crate::{
    app::App,
    error::Code,
    owner::OwnerLink,
    run_config::RunConfig,
    sandbox::{self, LONG_POLL_MS},
    store::{Lease, load_item, upsert_item},
};

/// A sandbox key is replaced when it has less than this left.
const KEY_REFRESH_MARGIN: chrono::TimeDelta = chrono::TimeDelta::minutes(5);
/// A harness request that has not completed after this long is treated as lost.
const HARNESS_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) async fn run_settings(app: &App, lease: &Lease, run: Uuid) -> anyhow::Result<RunConfig> {
    let mut tx = app.store.tx(lease.org_id).await?;
    let value: Value = sqlx::query_scalar("SELECT run_config FROM runs WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    RunConfig::resolve(value, "work", &app.config)
}

/// Send a harness request and wait for its normalized result.
pub(crate) async fn harness(
    app: &App,
    lease: &Lease,
    sandbox_id: Uuid,
    op_id: String,
    request: HarnessRequest,
) -> anyhow::Result<HarnessResult> {
    let deadline = tokio::time::Instant::now() + HARNESS_TIMEOUT;
    let mut reply = sandbox::guest(
        app,
        lease,
        sandbox_id,
        GuestCommand::Harness {
            op_id: op_id.clone(),
            request,
        },
    )
    .await?;
    while reply["state"] == "running" {
        ensure!(
            tokio::time::Instant::now() < deadline,
            Code::NativeOutcomeUnknown
        );
        let poll = GuestCommand::Poll {
            op_id: op_id.clone(),
            wait_ms: LONG_POLL_MS,
        };
        reply = sandbox::guest(app, lease, sandbox_id, poll).await?;
    }
    match reply["state"].as_str() {
        Some("completed") => {
            Ok(serde_json::from_value(reply["result"].clone()).unwrap_or_default())
        }
        Some("failed") => anyhow::bail!(
            "harness request failed: {}",
            reply["error"].as_str().unwrap_or("unknown")
        ),
        _ => anyhow::bail!(Code::NativeOutcomeUnknown),
    }
}

/// Drive one run: open (or continue) the native thread, deliver inputs and answers, and
/// project harness events until the run's turn completes.
pub async fn drive(app: App, lease: Lease, run: Uuid, link: OwnerLink) -> anyhow::Result<()> {
    let sandbox_id = ensure_work(&app, &lease, run).await?;
    let mut key_expires = Utc::now() + chrono::TimeDelta::hours(1);
    let settings = run_settings(&app, &lease, run).await?;
    let thread = open_thread(&app, &lease, run, sandbox_id, &settings).await?;
    let mut acked: u64 = acked_gseq(&app, &lease, sandbox_id).await?;
    let mut folds_seen = 0;
    loop {
        if key_expires - Utc::now() < KEY_REFRESH_MARGIN {
            key_expires = refresh_key(&app, &lease, run, sandbox_id).await?;
        }
        crate::mcp::project(&app, &lease, run).await?;
        if !deliver(&app, &lease, run, sandbox_id, &thread, &settings).await? {
            return Ok(());
        }
        let replay = tokio::select! {
            reply = sandbox::guest(&app, &lease, sandbox_id, GuestCommand::Replay {
                after: acked,
                wait_ms: LONG_POLL_MS,
                folds_seen,
            }) => Some(reply?),
            // New inputs, answers or a cancellation: deliver them now.
            () = link.commands_processed() => None,
        };
        if let Some(reply) = replay {
            folds_seen = reply["fold_version"].as_u64().unwrap_or(0);
            if projection::apply(&app, &lease, run, sandbox_id, &reply, None, &link).await? {
                return Ok(());
            }
            acked = acked_gseq(&app, &lease, sandbox_id).await?;
        }
    }
}

async fn acked_gseq(app: &App, lease: &Lease, sandbox_id: Uuid) -> anyhow::Result<u64> {
    let mut tx = app.store.tx(lease.org_id).await?;
    let acked: i64 =
        sqlx::query_scalar("SELECT acked_gseq FROM sandboxes WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(sandbox_id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(acked as u64)
}

/// Resume the sandbox's native thread, fork it from a parent conversation, or start one.
async fn open_thread(
    app: &App,
    lease: &Lease,
    run: Uuid,
    sandbox_id: Uuid,
    settings: &RunConfig,
) -> anyhow::Result<String> {
    let (thread, fork): (Option<String>, Option<Value>) = {
        let mut tx = app.store.tx(lease.org_id).await?;
        let row = sqlx::query(
            "SELECT s.native_thread_id,c.native_fork FROM sandboxes s
             JOIN conversations c ON c.org_id=s.org_id AND c.id=s.conversation_id
             WHERE s.org_id=$1 AND s.id=$2",
        )
        .bind(lease.org_id)
        .bind(sandbox_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        (row.get("native_thread_id"), row.get("native_fork"))
    };
    let instructions = {
        let mut tx = app.store.tx(lease.org_id).await?;
        let instructions = crate::projects::instructions(&mut tx, lease.org_id, run).await?;
        tx.commit().await?;
        instructions
    };
    let turn = session::turn_settings(settings);
    let thread = match (fork, thread) {
        (Some(fork), _) => {
            let request = HarnessRequest::ForkThread {
                thread_id: fork["manifest"]["thread_id"]
                    .as_str()
                    .context("fork without thread")?
                    .into(),
                last_turn_id: fork["last_turn_id"].as_str().map(str::to_owned),
                message_id: fork["message_id"].as_str().map(str::to_owned),
                settings: turn.clone(),
                instructions: instructions.clone(),
            };
            let thread = harness(
                app,
                lease,
                sandbox_id,
                format!("{sandbox_id}:fork"),
                request,
            )
            .await?
            .thread_id
            .context("harness returned no thread")?;
            let mut tx = app.store.fenced(lease).await?;
            sqlx::query("UPDATE sandboxes SET native_thread_id=$3 WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(sandbox_id)
                .bind(&thread)
                .execute(&mut *tx)
                .await?;
            // The fork's pin is keyed by the forked conversation.
            sqlx::query("DELETE FROM restore_pins WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(lease.conversation_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE conversations SET native_fork=NULL WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(lease.conversation_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            thread
        }
        (None, Some(thread)) => {
            let request = HarnessRequest::ResumeThread {
                thread_id: thread.clone(),
                settings: turn.clone(),
                instructions: instructions.clone(),
            };
            harness(app, lease, sandbox_id, format!("{run}:resume"), request).await?;
            thread
        }
        (None, None) => {
            let request = HarnessRequest::StartThread {
                settings: turn.clone(),
                instructions: instructions.clone(),
            };
            let thread = harness(
                app,
                lease,
                sandbox_id,
                format!("{sandbox_id}:thread"),
                request,
            )
            .await?
            .thread_id
            .context("harness returned no thread")?;
            let mut tx = app.store.fenced(lease).await?;
            sqlx::query("UPDATE sandboxes SET native_thread_id=$3 WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(sandbox_id)
                .bind(&thread)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            thread
        }
    };
    let configure = HarnessRequest::Configure {
        settings: turn,
        instructions,
    };
    harness(app, lease, sandbox_id, format!("{run}:config"), configure).await?;
    Ok(thread)
}

/// Deliver answered approvals/questions and pending inputs. Returns false once the run is no
/// longer active (its current native turn is interrupted first).
async fn deliver(
    app: &App,
    lease: &Lease,
    run: Uuid,
    sandbox_id: Uuid,
    thread: &str,
    settings: &RunConfig,
) -> anyhow::Result<bool> {
    let (state, turn, compact, inputs, answers, recovered) = {
        let mut tx = app.store.fenced(lease).await?;
        let row = sqlx::query(
            "SELECT state,native_turn_id,operation FROM runs WHERE org_id=$1 AND id=$2",
        )
        .bind(lease.org_id)
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
        let inputs: Vec<(i64, String, String, Uuid)> = sqlx::query_as(
            "SELECT id,text,kind,item_id FROM pending_inputs WHERE org_id=$1 AND run_id=$2 AND consumed_step IS NULL ORDER BY id",
        )
        .bind(lease.org_id)
        .bind(run)
        .fetch_all(&mut *tx)
        .await?;
        let answers: Vec<(String, Value)> = sqlx::query_as(
            "SELECT request_id,answer FROM native_requests WHERE org_id=$1 AND sandbox_id=$2
             AND answer IS NOT NULL AND NOT delivered",
        )
        .bind(lease.org_id)
        .bind(sandbox_id)
        .fetch_all(&mut *tx)
        .await?;
        let recovered: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspace_jobs WHERE org_id=$1 AND id=$2 AND kind='restore')",
        )
        .bind(lease.org_id)
        .bind(sandbox_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        (
            row.get::<String, _>("state"),
            row.get::<Option<String>, _>("native_turn_id"),
            row.get::<&str, _>("operation") == "compact",
            inputs,
            answers,
            recovered,
        )
    };
    if !matches!(state.as_str(), "running" | "requires_action") {
        if let Some(turn) = turn {
            interrupt(app, lease, run, sandbox_id, thread, &turn).await?;
        }
        return Ok(false);
    }
    for (request, answer) in answers {
        let answer: HarnessAnswer = serde_json::from_value(answer)?;
        sandbox::guest(
            app,
            lease,
            sandbox_id,
            GuestCommand::Answer {
                op_id: format!("answer:{request}"),
                request_id: request.clone(),
                answer,
            },
        )
        .await?;
        let mut tx = app.store.fenced(lease).await?;
        sqlx::query("UPDATE native_requests SET delivered=true WHERE org_id=$1 AND sandbox_id=$2 AND request_id=$3")
            .bind(lease.org_id)
            .bind(sandbox_id)
            .bind(request)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    let claude = settings.harness == "claude";
    for (seq, text, kind, item) in inputs {
        let steer = kind == "steer";
        // Claude has no native steer: steers wait for the next turn of the same run.
        let queued = steer && claude;
        if steer && (queued == turn.is_some()) {
            set_steer_state(app, lease, item, "waiting").await?;
            continue;
        }
        let request = if !steer || queued {
            let text = if recovered {
                format!(
                    "[平台恢复说明：工作区与原生会话已恢复到已提交检查点。水位之后的操作已标记失效，不应假定它们仍然生效。]\n{text}"
                )
            } else {
                text
            };
            if compact && !claude {
                HarnessRequest::Compact {
                    thread_id: thread.into(),
                }
            } else {
                HarnessRequest::StartTurn {
                    thread_id: thread.into(),
                    text,
                    settings: session::turn_settings(settings),
                }
            }
        } else {
            HarnessRequest::SteerTurn {
                thread_id: thread.into(),
                turn_id: turn.clone().unwrap_or_default(),
                text,
            }
        };
        let starts_turn = matches!(request, HarnessRequest::StartTurn { .. });
        let result = harness(
            app,
            lease,
            sandbox_id,
            format!("{run}:input:{seq}"),
            request,
        )
        .await?;
        let mut tx = app.store.fenced(lease).await?;
        if starts_turn {
            let new_turn = result.turn_id.context("harness returned no turn")?;
            sqlx::query("UPDATE runs SET native_turn_id=$3 WHERE org_id=$1 AND id=$2")
                .bind(lease.org_id)
                .bind(run)
                .bind(&new_turn)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO native_turns(org_id,sandbox_id,turn_id,run_id) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",
            )
            .bind(lease.org_id)
            .bind(sandbox_id)
            .bind(&new_turn)
            .bind(run)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE pending_inputs SET consumed_step=0 WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(seq)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if steer {
            set_steer_state(app, lease, item, "handed").await?;
        }
        // A new turn changes how the remaining steers are delivered; reload first.
        if !steer || queued {
            break;
        }
    }
    Ok(true)
}

async fn set_steer_state(app: &App, lease: &Lease, item: Uuid, state: &str) -> anyhow::Result<()> {
    let mut tx = app.store.fenced(lease).await?;
    let mut steer = load_item(&mut tx, lease.org_id, item).await?;
    if steer.state != state {
        steer.revision += 1;
        steer.state = state.into();
        upsert_item(&mut tx, lease, steer).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn interrupt(
    app: &App,
    lease: &Lease,
    run: Uuid,
    sandbox_id: Uuid,
    thread: &str,
    turn: &str,
) -> anyhow::Result<()> {
    let request = HarnessRequest::InterruptTurn {
        thread_id: thread.into(),
        turn_id: turn.into(),
    };
    let _ = harness(app, lease, sandbox_id, format!("{run}:interrupt"), request).await;
    let mut tx = app.store.tx(lease.org_id).await?;
    sqlx::query("UPDATE runs SET native_interrupted=true WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(run)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Interrupt the native turn of a run that ended while no worker was driving it (for
/// example after an Owner restart). Each run is interrupted once.
pub async fn interrupt_terminal(app: &App, lease: &Lease) -> anyhow::Result<()> {
    let rows: Vec<(Uuid, Uuid, String, String)> = {
        let mut tx = app.store.tx(lease.org_id).await?;
        let rows = sqlx::query_as(
            "SELECT s.id,r.id,s.native_thread_id,r.native_turn_id FROM sandboxes s
             JOIN runs r ON r.org_id=s.org_id AND r.id=s.run_id
             WHERE s.org_id=$1 AND s.conversation_id=$2 AND s.persistent AND s.state='running'
               AND r.state IN ('cancelled','failed') AND NOT r.native_interrupted
               AND s.native_thread_id IS NOT NULL AND r.native_turn_id IS NOT NULL",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows
    };
    for (sandbox_id, run, thread, turn) in rows {
        sandbox::guest(app, lease, sandbox_id, GuestCommand::Fence).await?;
        interrupt(app, lease, run, sandbox_id, &thread, &turn).await?;
    }
    Ok(())
}

/// Project guest events up to `through` (a checkpoint watermark) for `run`.
pub async fn sync_through(
    app: &App,
    lease: &Lease,
    run: Uuid,
    sandbox_id: Uuid,
    through: u64,
    link: &OwnerLink,
) -> anyhow::Result<()> {
    loop {
        let acked = acked_gseq(app, lease, sandbox_id).await?;
        if acked >= through {
            return Ok(());
        }
        let replay = GuestCommand::Replay {
            after: acked,
            wait_ms: 0,
            folds_seen: 0,
        };
        let reply = sandbox::guest(app, lease, sandbox_id, replay).await?;
        projection::apply(app, lease, run, sandbox_id, &reply, Some(through), link).await?;
        if acked_gseq(app, lease, sandbox_id).await? == acked {
            // The next event belongs to a turn that is not registered yet.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
