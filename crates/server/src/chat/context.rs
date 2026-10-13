//! Model context as a chain of loop steps.
//!
//! A step stores either a full snapshot of its request (`request_messages`) or a pointer to the
//! step it continues plus only the inputs added since (`input_delta`). The context after a
//! completed step is its request, its native response and the results of its tool calls.
//! A snapshot is written every [`SNAPSHOT_EVERY`] links and after a compaction, so storage
//! grows linearly and a reconstruction reads at most that many rows.
use anyhow::{Context, ensure};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

use super::protocol;
use crate::store::Tx;

pub const SNAPSHOT_EVERY: i32 = 32;

#[derive(sqlx::FromRow)]
struct Link {
    run_id: Uuid,
    step_no: i32,
    request_messages: Option<Value>,
    input_delta: Option<Value>,
    native_response: Option<Value>,
    protocol: String,
    chain_length: i32,
}

impl Link {
    fn resets_context(&self) -> bool {
        self.native_response
            .as_ref()
            .is_some_and(protocol::resets_context)
    }
}

/// Links from the nearest snapshot to `(run, step)`, oldest first.
async fn chain(tx: &mut Tx<'_>, org: Uuid, run: Uuid, step: i32) -> anyhow::Result<Vec<Link>> {
    let links: Vec<Link> = sqlx::query_as(
        "WITH RECURSIVE chain AS (
             SELECT run_id,step_no,base_run_id,base_step,request_messages,input_delta,native_response,
                    protocol,chain_length,0 AS depth
             FROM loop_steps WHERE org_id=$1 AND run_id=$2 AND step_no=$3
           UNION ALL
             SELECT s.run_id,s.step_no,s.base_run_id,s.base_step,s.request_messages,s.input_delta,
                    s.native_response,s.protocol,s.chain_length,c.depth+1
             FROM loop_steps s JOIN chain c ON s.org_id=$1 AND s.run_id=c.base_run_id AND s.step_no=c.base_step
             WHERE c.request_messages IS NULL)
         SELECT run_id,step_no,request_messages,input_delta,native_response,protocol,chain_length
         FROM chain ORDER BY depth DESC",
    )
    .bind(org)
    .bind(run)
    .bind(step)
    .fetch_all(&mut **tx)
    .await?;
    ensure!(
        links
            .first()
            .is_some_and(|link| link.request_messages.is_some()),
        "loop step {run}:{step} has no snapshot"
    );
    Ok(links)
}

/// Completed tool results per (run, step), ordered by call id as the provider expects.
async fn results(
    tx: &mut Tx<'_>,
    org: Uuid,
    links: &[Link],
) -> anyhow::Result<HashMap<(Uuid, i32), Vec<(String, Value)>>> {
    let runs: Vec<Uuid> = links.iter().map(|link| link.run_id).collect();
    let rows: Vec<(Uuid, i32, String, Value)> = sqlx::query_as(
        "SELECT run_id,step_no,call_id,result FROM tool_calls
         WHERE org_id=$1 AND run_id=ANY($2) AND status='completed' ORDER BY call_id",
    )
    .bind(org)
    .bind(&runs)
    .fetch_all(&mut **tx)
    .await?;
    let mut map: HashMap<(Uuid, i32), Vec<(String, Value)>> = HashMap::new();
    for (run, step, call, result) in rows {
        map.entry((run, step)).or_default().push((call, result));
    }
    Ok(map)
}

/// Walk the chain, returning the request of the last link and the context after each link
/// (the latter only for the final one).
async fn walk(
    tx: &mut Tx<'_>,
    org: Uuid,
    run: Uuid,
    step: i32,
) -> anyhow::Result<(Value, Value, Link)> {
    let mut links = chain(tx, org, run, step).await?;
    let results = results(tx, org, &links).await?;
    let mut request = links[0].request_messages.clone().context("snapshot")?;
    let mut after = continued(&links[0], request.clone(), &results)?;
    for link in &links[1..] {
        request = after;
        let delta = link
            .input_delta
            .as_ref()
            .and_then(Value::as_array)
            .context("delta")?;
        request
            .as_array_mut()
            .context("context is not a message list")?
            .extend(delta.iter().cloned());
        after = continued(link, request.clone(), &results)?;
    }
    let last = links.pop().context("empty chain")?;
    Ok((request, after, last))
}

fn continued(
    link: &Link,
    mut messages: Value,
    results: &HashMap<(Uuid, i32), Vec<(String, Value)>>,
) -> anyhow::Result<Value> {
    if let Some(response) = &link.native_response {
        let empty = vec![];
        let calls = results.get(&(link.run_id, link.step_no)).unwrap_or(&empty);
        protocol::continue_with(&link.protocol, &mut messages, response, calls)?;
    }
    Ok(messages)
}

/// The model input of step `(run, step)`.
pub async fn request_messages(
    tx: &mut Tx<'_>,
    org: Uuid,
    run: Uuid,
    step: i32,
) -> anyhow::Result<Value> {
    Ok(walk(tx, org, run, step).await?.0)
}

/// The context after step `(run, step)`: its request, its response and completed tool results.
/// Tool calls without a result are omitted and an open provider turn is closed, so this is also
/// a valid prefix for a new user message while the step's tools are still pending.
pub async fn committed_prefix(
    tx: &mut Tx<'_>,
    org: Uuid,
    run: Uuid,
    step: i32,
) -> anyhow::Result<Value> {
    let (_, mut after, last) = walk(tx, org, run, step).await?;
    protocol::close_turn(&last.protocol, &mut after);
    Ok(after)
}

/// How a new step is stored, and its full model input.
pub struct NewStep {
    pub messages: Value,
    pub snapshot: Option<Value>,
    pub base: Option<(Uuid, i32)>,
    pub delta: Option<Value>,
    pub chain_length: i32,
}

/// Plan a step that continues `base` (or starts from `initial`) with `inputs`.
pub async fn plan_step(
    tx: &mut Tx<'_>,
    org: Uuid,
    base: Option<(Uuid, i32)>,
    initial: Value,
    inputs: Vec<Value>,
) -> anyhow::Result<NewStep> {
    let Some((run, step)) = base else {
        let mut messages = initial;
        messages
            .as_array_mut()
            .context("seed is not a message list")?
            .extend(inputs);
        return Ok(NewStep {
            snapshot: Some(messages.clone()),
            messages,
            base: None,
            delta: None,
            chain_length: 0,
        });
    };
    let (_, mut messages, link) = walk(tx, org, run, step).await?;
    messages
        .as_array_mut()
        .context("context is not a message list")?
        .extend(inputs.iter().cloned());
    let chain_length = link.chain_length + 1;
    Ok(if chain_length >= SNAPSHOT_EVERY || link.resets_context() {
        NewStep {
            snapshot: Some(messages.clone()),
            messages,
            base: None,
            delta: None,
            chain_length: 0,
        }
    } else {
        NewStep {
            messages,
            snapshot: None,
            base: Some((run, step)),
            delta: Some(Value::Array(inputs)),
            chain_length,
        }
    })
}
