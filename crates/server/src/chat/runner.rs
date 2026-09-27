//! The Chat agent loop: plan a step, call the model through the gateway, record tool calls,
//! execute them, repeat. Every step is checkpointed in `loop_steps`, so a new Owner resumes
//! from the last committed step instead of from the timeline.
use anyhow::{Context, ensure};
use futures_util::StreamExt;
use ryzhub_proto::{Item, ItemDetail, ToolDetail, event_envelope::Event, item_detail};
use serde_json::{Value, json};
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

use super::{context, protocol, tools};
use crate::{
    app::App,
    error::{Code, code_of, or_code},
    llm::keys,
    owner::{LiveRef, OwnerLink, WorkerEvent},
    provider::{NativeMessage, SseDecoder},
    run_config::RunConfig,
    runs::{finish, guard},
    store::{Lease, append, load_item, upsert_item},
};

/// Model steps per run, and attempts per step.
const MAX_STEPS: i32 = 32;
const MAX_ATTEMPTS: i32 = 3;
const MAX_TOOL_CALLS: usize = 32;

struct Step {
    number: i32,
    messages: Value,
    item: Item,
    user: Uuid,
    read_only: bool,
    attempt: i32,
    config: RunConfig,
    instructions: String,
    compact: bool,
}

pub async fn drive(app: App, lease: Lease, run: Uuid, link: OwnerLink) -> anyhow::Result<()> {
    loop {
        if !tools::advance(&app, &lease, run, &link).await? {
            return Ok(());
        }
        let Some(step) = prepare(&app, &lease, run, &link).await? else {
            return Ok(());
        };
        match model(&app, &lease, run, &step, &link).await {
            Ok(response) => complete(&app, &lease, run, step, response, &link).await?,
            Err(error) => {
                let code = code_of(&error);
                if step.attempt >= MAX_ATTEMPTS || !transient(code) {
                    return Err(error);
                }
                let backoff = if code == Some(Code::RateLimited) {
                    2000
                } else {
                    200
                };
                tokio::time::sleep(Duration::from_millis(backoff * step.attempt as u64)).await;
            }
        }
    }
}

/// Failures worth another attempt of the same step.
fn transient(code: Option<Code>) -> bool {
    matches!(
        code,
        None | Some(
            Code::ModelConnectionFailed
                | Code::ModelStreamInterrupted
                | Code::UpstreamStreamError
                | Code::UpstreamStreamInterrupted
                | Code::UpstreamUnavailable
                | Code::RateLimited
        )
    )
}

/// Resume a planned step, or plan the next one from the last completed step and the pending
/// inputs. Returns None when the run has finished.
async fn prepare(
    app: &App,
    lease: &Lease,
    run: Uuid,
    link: &OwnerLink,
) -> anyhow::Result<Option<Step>> {
    let org = lease.org_id;
    let mut tx = app.store.tx(org).await?;
    guard(&mut tx, lease, run).await?;
    let row = sqlx::query(
        "SELECT r.assistant_item_id,r.read_only,r.seed_messages,r.seed_run_id,r.seed_step,r.run_config,
                r.operation,c.creator_id
         FROM runs r JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id
         WHERE r.org_id=$1 AND r.id=$2",
    )
    .bind(org)
    .bind(run)
    .fetch_one(&mut *tx)
    .await?;
    let config: RunConfig = serde_json::from_value(row.get("run_config"))?;
    let read_only: bool = row.get("read_only");
    let compact = row.get::<&str, _>("operation") == "compact";
    let last: Option<(i32, String, i32)> = sqlx::query_as(
        "SELECT step_no,status,attempt FROM loop_steps WHERE org_id=$1 AND run_id=$2
         ORDER BY step_no DESC LIMIT 1",
    )
    .bind(org)
    .bind(run)
    .fetch_optional(&mut *tx)
    .await?;
    let (number, messages, attempt) = match last {
        Some((number, status, attempt)) if status == "planned" => {
            let messages = context::request_messages(&mut tx, org, run, number).await?;
            (number, messages, attempt)
        }
        Some((number, _, _)) => {
            let called: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM tool_calls WHERE org_id=$1 AND run_id=$2 AND step_no=$3)",
            )
            .bind(org)
            .bind(run)
            .bind(number)
            .fetch_one(&mut *tx)
            .await?;
            let pending: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pending_inputs WHERE org_id=$1 AND run_id=$2 AND consumed_step IS NULL)",
            )
            .bind(org)
            .bind(run)
            .fetch_one(&mut *tx)
            .await?;
            // A final answer with no steer waiting ends the run.
            if !called && (!pending || read_only) {
                finish(&mut tx, lease, run, "completed", None).await?;
                tx.commit().await?;
                link.committed().await;
                return Ok(None);
            }
            let next = number + 1;
            ensure!(next < MAX_STEPS, Code::LoopLimitReached);
            let messages = plan(
                &mut tx,
                lease,
                run,
                next,
                Some((run, number)),
                Value::Null,
                compact,
            )
            .await?;
            (next, messages, 0)
        }
        None => {
            let seed_run: Option<Uuid> = row.get("seed_run_id");
            let base = seed_run.zip(row.get::<Option<i32>, _>("seed_step"));
            let messages = plan(
                &mut tx,
                lease,
                run,
                0,
                base,
                row.get("seed_messages"),
                compact,
            )
            .await?;
            (0, messages, 0)
        }
    };
    ensure!(attempt < MAX_ATTEMPTS, Code::LoopLimitReached);
    let mut item = if number == 0 || attempt > 0 {
        load_item(&mut tx, org, row.get("assistant_item_id")).await?
    } else {
        Item {
            id: Uuid::now_v7().to_string(),
            kind: if read_only { "side_query" } else { "assistant" }.into(),
            run_id: run.to_string(),
            ..Default::default()
        }
    };
    item.revision += 1;
    item.text.clear();
    item.state = "streaming".into();
    let item = upsert_item(&mut tx, lease, item).await?;
    let item_id = Uuid::parse_str(&item.id)?;
    sqlx::query("UPDATE runs SET assistant_item_id=$3,response_text='' WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(run)
        .bind(item_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE loop_steps SET attempt=attempt+1,item_id=$4 WHERE org_id=$1 AND run_id=$2 AND step_no=$3",
    )
    .bind(org)
    .bind(run)
    .bind(number)
    .bind(item_id)
    .execute(&mut *tx)
    .await?;
    let instructions = crate::projects::instructions(&mut tx, org, run).await?;
    tx.commit().await?;
    link.committed().await;
    Ok(Some(Step {
        number,
        messages,
        item,
        user: row.get("creator_id"),
        read_only,
        attempt: attempt + 1,
        config,
        instructions,
        compact,
    }))
}

/// Consume pending inputs into a new planned step. Returns the step's full model input.
async fn plan(
    tx: &mut crate::store::Tx<'_>,
    lease: &Lease,
    run: Uuid,
    number: i32,
    base: Option<(Uuid, i32)>,
    initial: Value,
    compact: bool,
) -> anyhow::Result<Value> {
    let org = lease.org_id;
    let inputs: Vec<(i64, String, String, Uuid)> = sqlx::query_as(
        "SELECT id,text,kind,item_id FROM pending_inputs WHERE org_id=$1 AND run_id=$2
         AND consumed_step IS NULL ORDER BY id FOR UPDATE",
    )
    .bind(org)
    .bind(run)
    .fetch_all(&mut **tx)
    .await?;
    let mut messages = vec![];
    let mut consumed = 0;
    for (id, text, kind, item) in inputs {
        consumed = id;
        // A compaction request is an operation, not a message the model should see.
        if !compact {
            messages.push(json!({"role":"user","content":text}));
        }
        if kind == "steer" {
            let mut item = load_item(tx, org, item).await?;
            item.revision += 1;
            item.state = "waiting".into();
            upsert_item(tx, lease, item).await?;
        }
    }
    sqlx::query(
        "UPDATE pending_inputs SET consumed_step=$3 WHERE org_id=$1 AND run_id=$2
         AND consumed_step IS NULL AND id<=$4",
    )
    .bind(org)
    .bind(run)
    .bind(number)
    .bind(consumed)
    .execute(&mut **tx)
    .await?;
    let initial = if initial.is_null() {
        json!([])
    } else {
        initial
    };
    let step = context::plan_step(tx, org, base, initial, messages).await?;
    let protocol: String =
        sqlx::query_scalar("SELECT protocol FROM runs WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(run)
            .fetch_one(&mut **tx)
            .await?;
    sqlx::query(
        "INSERT INTO loop_steps(org_id,run_id,step_no,request_messages,base_run_id,base_step,input_delta,
             chain_length,consumed_input_seq,status,attempt,protocol)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,'planned',0,$10)",
    )
    .bind(org)
    .bind(run)
    .bind(number)
    .bind(&step.snapshot)
    .bind(step.base.map(|(run, _)| run))
    .bind(step.base.map(|(_, step)| step))
    .bind(&step.delta)
    .bind(step.chain_length)
    .bind(consumed)
    .bind(protocol)
    .execute(&mut **tx)
    .await?;
    Ok(step.messages)
}

fn request_body(app: &App, step: &Step) -> Value {
    let system = format!(
        "You are a helpful assistant. Use ask_user when a decision is needed. Execution tools run only in a sandbox; only /workspace persists. Never claim a tool succeeded without its result.{}",
        step.instructions
    );
    let responses = step.config.protocol == "responses";
    let mut body = if responses {
        json!({"model":step.config.model,"stream":true,"store":false,"input":step.messages,"instructions":system,
               "include":["reasoning.encrypted_content"],"reasoning":{"effort":step.config.reasoning}})
    } else {
        json!({"model":step.config.model,"max_tokens":app.config.llm.max_tokens,"stream":true,
               "messages":step.messages,"system":system})
    };
    if !step.read_only && !step.compact {
        let definitions = tools::definitions();
        body["tools"] = if responses {
            json!(
                definitions
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|d| json!({
                        "type":"function","name":d["name"],"description":d["description"],
                        "parameters":d["input_schema"],"strict":false
                    }))
                    .collect::<Vec<_>>()
            )
        } else {
            definitions
        };
    }
    if step.compact {
        let object = body.as_object_mut().unwrap();
        for key in ["stream", "store", "include", "reasoning"] {
            object.remove(key);
        }
    }
    body
}

/// Map a gateway error response to a stable code without copying upstream payloads.
fn gateway_error(status: reqwest::StatusCode, body: &Value) -> Code {
    match body["error"].as_str().and_then(Code::parse) {
        Some(
            code @ (Code::Fenced
            | Code::ModelDenied
            | Code::CredentialUnavailable
            | Code::CredentialRevoked
            | Code::PlatformCredentialDenied
            | Code::RateLimited
            | Code::UpstreamUnavailable),
        ) => code,
        _ if status == reqwest::StatusCode::UNAUTHORIZED => Code::ModelAuthenticationFailed,
        _ if status == reqwest::StatusCode::TOO_MANY_REQUESTS => Code::RateLimited,
        _ if status.is_server_error() => Code::UpstreamUnavailable,
        _ => Code::ModelRequestFailed,
    }
}

async fn model(
    app: &App,
    lease: &Lease,
    run: Uuid,
    step: &Step,
    link: &OwnerLink,
) -> anyhow::Result<Value> {
    let key = keys::issue_loop_key(
        &app.store,
        lease,
        step.user,
        run,
        vec![step.config.model.clone()],
    )
    .await?;
    let responses = step.config.protocol == "responses";
    let path = if step.compact {
        "responses/compact"
    } else if responses {
        "responses"
    } else {
        "messages"
    };
    let response = app
        .http
        .gateway
        .post(format!(
            "{}/v1/{path}",
            app.config.llm.gateway_url.trim_end_matches('/')
        ))
        .bearer_auth(key)
        .header("anthropic-version", "2023-06-01")
        .json(&request_body(app, step))
        .send()
        .await
        .context(Code::ModelConnectionFailed)?;
    if !response.status().is_success() {
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        anyhow::bail!(gateway_error(status, &body));
    }
    if step.compact {
        let value: Value = response.json().await?;
        ensure!(value["output"].is_array(), Code::ModelRequestFailed);
        return Ok(value);
    }
    mark_steers_handed(app, lease, run, step.number, link).await?;
    let live = LiveRef::of(&step.item);
    let mut stream = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut anthropic = NativeMessage::default();
    let mut openai = protocol::NativeResponse::default();
    let mut offset = 0;
    // A stream the decoders reject is an upstream fault: retried, then reported as such.
    let invalid = |error| or_code(error, Code::UpstreamStreamError);
    while let Some(chunk) = stream.next().await {
        for event in decoder
            .push(&chunk.context(Code::ModelStreamInterrupted)?)
            .map_err(invalid)?
        {
            let delta = if responses {
                openai.apply(&event)
            } else {
                anthropic.apply(&event)
            }
            .map_err(invalid)?;
            if let Some(text) = delta {
                let len = text.len();
                link.send(WorkerEvent::Append {
                    item: live.clone(),
                    offset,
                    text,
                })
                .await?;
                offset += len;
            }
        }
    }
    if responses {
        openai.finish()
    } else {
        anthropic.finish()
    }
    .map_err(invalid)
}

/// Steers consumed by this step are now part of a model request.
async fn mark_steers_handed(
    app: &App,
    lease: &Lease,
    run: Uuid,
    step: i32,
    link: &OwnerLink,
) -> anyhow::Result<()> {
    let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
    let steers: Vec<Uuid> = sqlx::query_scalar(
        "SELECT item_id FROM pending_inputs WHERE org_id=$1 AND run_id=$2 AND kind='steer' AND consumed_step=$3",
    )
    .bind(lease.org_id)
    .bind(run)
    .bind(step)
    .fetch_all(&mut *tx)
    .await?;
    for id in &steers {
        let mut item = load_item(&mut tx, lease.org_id, *id).await?;
        item.revision += 1;
        item.state = "handed".into();
        upsert_item(&mut tx, lease, item).await?;
    }
    tx.commit().await?;
    if !steers.is_empty() {
        link.committed().await;
    }
    Ok(())
}

/// Commit the step's native response, finalize its reply and plan its tool calls.
async fn complete(
    app: &App,
    lease: &Lease,
    run: Uuid,
    step: Step,
    response: Value,
    link: &OwnerLink,
) -> anyhow::Result<()> {
    let (text, calls) = protocol::visible_and_tools(&step.config.protocol, &response)
        .map_err(|error| or_code(error, Code::ModelRequestFailed))?;
    ensure!(
        !step.read_only || calls.is_empty(),
        Code::ReadOnlyToolRequestRejected
    );
    ensure!(calls.len() <= MAX_TOOL_CALLS, Code::TooManyToolCalls);
    let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
    sqlx::query(
        "UPDATE loop_steps SET status='completed',native_response=$4
         WHERE org_id=$1 AND run_id=$2 AND step_no=$3 AND status='planned'",
    )
    .bind(lease.org_id)
    .bind(run)
    .bind(step.number)
    .bind(&response)
    .execute(&mut *tx)
    .await?;
    let mut item = step.item;
    item.revision += 1;
    item.state = "completed".into();
    item.text = if step.compact {
        "上下文已压缩".into()
    } else {
        text.clone()
    };
    upsert_item(&mut tx, lease, item).await?;
    sqlx::query("UPDATE runs SET response_text=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(run)
        .bind(&text)
        .execute(&mut *tx)
        .await?;
    for call in calls {
        let id = call["id"].as_str().context("tool call id")?;
        let name = call["name"].as_str().context("tool name")?;
        ensure!(
            id.len() <= 256 && name.len() <= 128 && call["input"].is_object(),
            Code::ModelRequestFailed
        );
        let item = Uuid::now_v7();
        append(
            &mut tx,
            lease.org_id,
            &lease.channel(),
            Some(lease),
            "",
            Event::ItemUpserted(Item {
                id: item.to_string(),
                revision: 1,
                kind: "tool".into(),
                state: "planned".into(),
                text: format!("{name}\n{}", call["input"]),
                run_id: run.to_string(),
                detail: Some(ItemDetail {
                    kind: Some(item_detail::Kind::Tool(ToolDetail {
                        name: name.into(),
                        arguments_json: call["input"].to_string(),
                        ..Default::default()
                    })),
                }),
                ..Default::default()
            }),
        )
        .await?;
        sqlx::query(
            "INSERT INTO tool_calls(org_id,run_id,call_id,step_no,name,arguments,item_id,status)
             VALUES($1,$2,$3,$4,$5,$6,$7,'planned')",
        )
        .bind(lease.org_id)
        .bind(run)
        .bind(id)
        .bind(step.number)
        .bind(name)
        .bind(&call["input"])
        .bind(item)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    link.committed().await;
    Ok(())
}
