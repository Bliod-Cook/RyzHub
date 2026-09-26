use super::{Progress, commands, committed_item, finish, guard};
use crate::{
    api::App,
    llm::identity,
    provider::{NativeMessage, SseDecoder},
    store::{Lease, append},
};
use anyhow::{Context, ensure};
use futures_util::StreamExt;
use ryzhub_proto::{Item, event_envelope::Event};
use serde_json::{Value, json};
use sqlx::Row;
use tokio::sync::mpsc;
use uuid::Uuid;

struct Step {
    number: i32,
    messages: Value,
    item: Item,
    user: Uuid,
    read_only: bool,
    attempt: i32,
    config: crate::run_config::RunConfig,
    instructions: String,
    operation: String,
}
pub async fn drive(
    app: App,
    lease: Lease,
    run: Uuid,
    sender: mpsc::Sender<Progress>,
) -> anyhow::Result<()> {
    loop {
        if !super::tools::advance(&app, &lease, run).await? {
            return Ok(());
        }
        let Some(step) = prepare(&app, &lease, run).await? else {
            return Ok(());
        };
        let result = model(&app, &lease, run, &step, &sender).await;
        match result {
            Ok(response) => complete(&app, &lease, run, step, response).await?,
            Err(error) => {
                if step.attempt >= 3
                    || matches!(
                        error.to_string().as_str(),
                        "FENCED" | "MODEL_DENIED" | "CREDENTIAL_UNAVAILABLE"
                    )
                {
                    return Err(error);
                }
                tokio::time::sleep(std::time::Duration::from_millis(200 * step.attempt as u64))
                    .await;
            }
        }
    }
}
async fn prepare(app: &App, lease: &Lease, run: Uuid) -> anyhow::Result<Option<Step>> {
    let mut tx = app.store.tx(lease.org_id).await?;
    guard(&mut tx, lease, run).await?;
    let row=sqlx::query("SELECT r.assistant_item_id,r.read_only,r.seed_messages,r.run_config,r.operation,c.creator_id FROM runs r JOIN conversations c ON c.org_id=r.org_id AND c.id=r.conversation_id WHERE r.org_id=$1 AND r.id=$2").bind(lease.org_id).bind(run).fetch_one(&mut *tx).await?;
    let user = row.get("creator_id");
    let operation: String = row.get("operation");
    let config: crate::run_config::RunConfig = serde_json::from_value(row.get("run_config"))?;
    let instructions = crate::projects::instructions(&mut tx, lease.org_id, run).await?;
    let read_only: bool = row.get("read_only");
    let last=sqlx::query("SELECT step_no,request_messages,native_response,status,attempt FROM loop_steps WHERE org_id=$1 AND run_id=$2 ORDER BY step_no DESC LIMIT 1").bind(lease.org_id).bind(run).fetch_optional(&mut *tx).await?;
    let mut number = 0i32;
    let mut messages: Value = row.get("seed_messages");
    let mut planned = false;
    let mut attempt = 0;
    if let Some(last) = last {
        number = last.get("step_no");
        messages = last.get("request_messages");
        if last.get::<&str, _>("status") == "planned" {
            planned = true;
            attempt = last.get("attempt");
        } else {
            let response: Value = last.get("native_response");
            let results:Vec<(String,Value)>=sqlx::query_as("SELECT call_id,result FROM tool_calls WHERE org_id=$1 AND run_id=$2 AND step_no=$3 AND status='completed' ORDER BY call_id").bind(lease.org_id).bind(run).bind(number).fetch_all(&mut *tx).await?;
            super::protocol::continue_with(&config.protocol, &mut messages, &response, &results)?;
            if results.is_empty() {
                let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pending_inputs WHERE org_id=$1 AND run_id=$2 AND consumed_step IS NULL)").bind(lease.org_id).bind(run).fetch_one(&mut *tx).await?;
                if !pending || read_only {
                    finish(&mut tx, lease, run, "completed", None).await?;
                    tx.commit().await?;
                    return Ok(None);
                }
            }
            number += 1;
        }
    }
    ensure!(number < 32 && attempt < 3, "LOOP_LIMIT_REACHED");
    if !planned {
        let inputs=sqlx::query("SELECT id,text,kind,item_id FROM pending_inputs WHERE org_id=$1 AND run_id=$2 AND consumed_step IS NULL ORDER BY id FOR UPDATE").bind(lease.org_id).bind(run).fetch_all(&mut *tx).await?;
        let mut consumed = 0i64;
        for input in inputs {
            consumed = input.get("id");
            let text: String = input.get("text");
            if operation != "compact" {
                messages
                    .as_array_mut()
                    .context("INVALID_CHECKPOINT")?
                    .push(json!({"role":"user","content":text}));
            }
            if input.get::<&str, _>("kind") == "steer" {
                let mut item = commands::item(&mut tx, lease.org_id, input.get("item_id")).await?;
                item.revision += 1;
                item.state = "waiting".into();
                committed_item(&mut tx, lease, item).await?;
            }
        }
        sqlx::query("UPDATE pending_inputs SET consumed_step=$3 WHERE org_id=$1 AND run_id=$2 AND consumed_step IS NULL AND id<=$4").bind(lease.org_id).bind(run).bind(number).bind(consumed).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO loop_steps(org_id,run_id,step_no,request_messages,consumed_input_seq,status,attempt,protocol) VALUES($1,$2,$3,$4,$5,'planned',0,$6)").bind(lease.org_id).bind(run).bind(number).bind(&messages).bind(consumed).bind(&config.protocol).execute(&mut *tx).await?;
    }
    let mut item = if number == 0 || planned {
        commands::item(&mut tx, lease.org_id, row.get("assistant_item_id")).await?
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
    let item = committed_item(&mut tx, lease, item).await?;
    sqlx::query("UPDATE runs SET assistant_item_id=$3,response_text='' WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(run)
        .bind(Uuid::parse_str(&item.id)?)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE loop_steps SET attempt=attempt+1,item_id=$4 WHERE org_id=$1 AND run_id=$2 AND step_no=$3",
    )
    .bind(lease.org_id)
    .bind(run)
    .bind(number)
    .bind(Uuid::parse_str(&item.id)?)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(Step {
        number,
        messages,
        item,
        user,
        read_only,
        attempt: attempt + 1,
        config,
        instructions,
        operation,
    }))
}
async fn model(
    app: &App,
    lease: &Lease,
    run: Uuid,
    step: &Step,
    sender: &mpsc::Sender<Progress>,
) -> anyhow::Result<Value> {
    let mut claims = identity::loop_identity(lease, step.user, run, &app.config.llm);
    claims.models = vec![step.config.model.clone()];
    let token = identity::sign(&app.config.llm, &claims)?;
    let system = format!(
        "You are a helpful assistant. Use ask_user when a decision is needed. Execution tools run only in a sandbox; only /workspace persists. Never claim a tool succeeded without its result.{}",
        step.instructions
    );
    let responses = step.config.protocol == "responses";
    let mut body = if responses {
        json!({"model":step.config.model,"stream":true,"store":false,"input":step.messages,"instructions":system,"include":["reasoning.encrypted_content"],"reasoning":{"effort":step.config.reasoning}})
    } else {
        json!({"model":step.config.model,"max_tokens":app.config.llm.max_tokens,"stream":true,"messages":step.messages,"system":system})
    };
    if !step.read_only {
        let definitions = super::tools::definitions();
        body["tools"] = if responses {
            json!(definitions.as_array().unwrap().iter().map(|d|json!({"type":"function","name":d["name"],"description":d["description"],"parameters":d["input_schema"],"strict":false})).collect::<Vec<_>>())
        } else {
            definitions
        };
    }
    if step.operation == "compact" {
        body.as_object_mut().unwrap().remove("stream");
        body.as_object_mut().unwrap().remove("tools");
        body.as_object_mut().unwrap().remove("store");
        body.as_object_mut().unwrap().remove("include");
        body.as_object_mut().unwrap().remove("reasoning");
    }
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(app.config.llm.timeout_secs))
        .build()?;
    let response = client
        .post(format!(
            "{}/v1/{}",
            app.config.llm.gateway_url.trim_end_matches('/'),
            if step.operation == "compact" {
                "responses/compact"
            } else if responses {
                "responses"
            } else {
                "messages"
            }
        ))
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("MODEL_CONNECTION_FAILED")?;
    if !response.status().is_success() {
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        let code = body["error"].as_str().unwrap_or("MODEL_REQUEST_FAILED");
        // No arbitrary upstream payload is copied into the UI or logs.
        anyhow::bail!(
            "{}",
            match code {
                "FENCED" | "MODEL_DENIED" | "CREDENTIAL_UNAVAILABLE" | "RATE_LIMITED" => code,
                _ =>
                    if status == reqwest::StatusCode::UNAUTHORIZED {
                        "MODEL_AUTHENTICATION_FAILED"
                    } else {
                        "MODEL_REQUEST_FAILED"
                    },
            }
        );
    }
    if step.operation == "compact" {
        let value: Value = response.json().await?;
        ensure!(value["output"].is_array(), "INVALID_COMPACTION");
        return Ok(value);
    }
    let mut bytes = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut native = NativeMessage::default();
    let mut native_response = super::protocol::NativeResponse::default();
    {
        let mut tx = app.store.tx(lease.org_id).await?;
        guard(&mut tx, lease, run).await?;
        let ids:Vec<Uuid>=sqlx::query_scalar("SELECT item_id FROM pending_inputs WHERE org_id=$1 AND run_id=$2 AND kind='steer' AND consumed_step=$3").bind(lease.org_id).bind(run).bind(step.number).fetch_all(&mut *tx).await?;
        for id in ids {
            let mut item = commands::item(&mut tx, lease.org_id, id).await?;
            item.revision += 1;
            item.state = "handed".into();
            committed_item(&mut tx, lease, item).await?;
        }
        tx.commit().await?;
    }
    let mut text = String::new();
    while let Some(chunk) = bytes.next().await {
        for event in decoder.push(&chunk.context("MODEL_STREAM_INTERRUPTED")?)? {
            if let Some(delta) = if responses {
                native_response.apply(&event)?
            } else {
                native.apply(&event)?
            } {
                let offset = text.len();
                text.push_str(&delta);
                sender
                    .send(Progress {
                        item: step.item.clone(),
                        text: text.clone(),
                        offset,
                    })
                    .await
                    .context("FENCED")?;
            }
        }
    }
    if responses {
        native_response.finish()
    } else {
        native.finish()
    }
}
async fn complete(
    app: &App,
    lease: &Lease,
    run: Uuid,
    step: Step,
    response: Value,
) -> anyhow::Result<()> {
    let (text, tool_blocks) = super::protocol::visible_and_tools(&step.config.protocol, &response)?;
    ensure!(
        !step.read_only || tool_blocks.is_empty(),
        "READ_ONLY_TOOL_REQUEST_REJECTED"
    );
    ensure!(tool_blocks.len() <= 32, "TOO_MANY_TOOL_CALLS");
    let mut tx = app.store.tx(lease.org_id).await?;
    guard(&mut tx, lease, run).await?;
    sqlx::query("UPDATE loop_steps SET status='completed',native_response=$4 WHERE org_id=$1 AND run_id=$2 AND step_no=$3 AND status='planned'").bind(lease.org_id).bind(run).bind(step.number).bind(&response).execute(&mut *tx).await?;
    let mut item = step.item;
    item.revision += 1;
    item.state = "completed".into();
    item.text = if step.operation == "compact" {
        "上下文已压缩".into()
    } else {
        text.clone()
    };
    committed_item(&mut tx, lease, item).await?;
    sqlx::query("UPDATE runs SET response_text=$3 WHERE org_id=$1 AND id=$2")
        .bind(lease.org_id)
        .bind(run)
        .bind(text)
        .execute(&mut *tx)
        .await?;
    for tool in tool_blocks {
        let call = tool["id"].as_str().context("INVALID_TOOL_CALL")?;
        let name = tool["name"].as_str().context("INVALID_TOOL_CALL")?;
        ensure!(
            call.len() <= 256 && name.len() <= 128 && tool["input"].is_object(),
            "INVALID_TOOL_CALL"
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
                text: format!("{name}\n{}", tool["input"]),
                run_id: run.to_string(),
                ..Default::default()
            }),
        )
        .await?;
        sqlx::query("INSERT INTO tool_calls(org_id,run_id,call_id,step_no,name,arguments,item_id,status) VALUES($1,$2,$3,$4,$5,$6,$7,'planned')").bind(lease.org_id).bind(run).bind(call).bind(step.number).bind(name).bind(&tool["input"]).bind(item).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
