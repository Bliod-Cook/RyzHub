//! Provider-native continuation; UI Items are never used as model history.
use anyhow::{Context, ensure};
use serde_json::{Value, json};

use crate::error::Code;

pub fn continue_with(
    protocol: &str,
    messages: &mut Value,
    response: &Value,
    results: &[(String, Value)],
) -> anyhow::Result<()> {
    if protocol == "responses" && response["object"] == "response.compaction" {
        ensure!(response["output"].is_array(), "INVALID_CHECKPOINT");
        *messages = response["output"].clone();
        return Ok(());
    }
    let target = messages.as_array_mut().context("INVALID_CHECKPOINT")?;
    if protocol == "responses" {
        for item in response["output"]
            .as_array()
            .context("INVALID_CHECKPOINT")?
        {
            // A side fork taken while approval is pending omits unresolved calls.
            if item["type"] != "function_call"
                || results
                    .iter()
                    .any(|(id, _)| Some(id.as_str()) == item["call_id"].as_str())
            {
                target.push(item.clone());
            }
        }
        for (id, result) in results {
            target.push(
                json!({"type":"function_call_output","call_id":id,"output":result.to_string()}),
            );
        }
    } else {
        let content: Vec<_> = response["content"]
            .as_array()
            .context("INVALID_CHECKPOINT")?
            .iter()
            .filter(|b| {
                b["type"] != "tool_use"
                    || results
                        .iter()
                        .any(|(id, _)| Some(id.as_str()) == b["id"].as_str())
            })
            .cloned()
            .collect();
        if !content.is_empty() {
            target.push(json!({"role":"assistant","content":content}));
        }
        if !results.is_empty() {
            let results: Vec<Value> = results
                .iter()
                .map(|(id, result)| {
                    json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "is_error": result.get("error").is_some(),
                        "content": result.to_string(),
                    })
                })
                .collect();
            target.push(json!({"role": "user", "content": results}));
        }
    }
    Ok(())
}

pub fn visible_and_tools(protocol: &str, response: &Value) -> anyhow::Result<(String, Vec<Value>)> {
    if protocol == "responses" {
        let output = response["output"]
            .as_array()
            .context("INVALID_MODEL_RESPONSE")?;
        let mut text = String::new();
        let mut tools = vec![];
        for item in output {
            match item["type"].as_str().unwrap_or("") {
                "message" => {
                    if let Some(content) = item["content"].as_array() {
                        for c in content {
                            if c["type"] == "output_text" {
                                text.push_str(c["text"].as_str().unwrap_or(""));
                            }
                        }
                    }
                }
                "function_call" => {
                    let args: Value = serde_json::from_str(
                        item["arguments"].as_str().context("INVALID_TOOL_CALL")?,
                    )?;
                    tools.push(json!({"id":item["call_id"],"name":item["name"],"input":args}));
                }
                "reasoning" | "compaction" => {}
                _ => anyhow::bail!("UNSUPPORTED_RESPONSE_ITEM"),
            }
        }
        Ok((text, tools))
    } else {
        let content = response["content"]
            .as_array()
            .context("INVALID_MODEL_RESPONSE")?;
        Ok((
            content
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect(),
            content
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .cloned()
                .collect(),
        ))
    }
}

#[derive(Default)]
pub struct NativeResponse {
    response: Option<Value>,
    items: std::collections::BTreeMap<u64, Value>,
    bytes: usize,
}

impl NativeResponse {
    pub fn apply(&mut self, event: &Value) -> anyhow::Result<Option<String>> {
        match event["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                let text = event["delta"].as_str().context("INVALID_RESPONSE_DELTA")?;
                self.bytes += text.len();
                ensure!(self.bytes <= 16 * 1024 * 1024, Code::ContentTooLarge);
                return Ok(Some(text.into()));
            }
            "response.output_item.done" => {
                let index = event["output_index"]
                    .as_u64()
                    .context("INVALID_OUTPUT_INDEX")?;
                ensure!(index < 1024, "TOO_MANY_OUTPUT_ITEMS");
                self.items.insert(index, event["item"].clone());
            }
            "response.completed" => {
                let mut r = event["response"].clone();
                ensure!(
                    r["status"] == "completed" && r["id"].is_string(),
                    "INVALID_MODEL_RESPONSE"
                );
                if !self.items.is_empty() {
                    r["output"] = json!(self.items.values().collect::<Vec<_>>());
                }
                ensure!(r["output"].is_array(), "INVALID_MODEL_RESPONSE");
                self.response = Some(r);
            }
            "response.incomplete" | "response.failed" | "error" => {
                anyhow::bail!(Code::UpstreamStreamError)
            }
            _ => {}
        }
        Ok(None)
    }

    pub fn finish(self) -> anyhow::Result<Value> {
        self.response.context(Code::UpstreamStreamInterrupted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn responses_preserve_completed_reasoning_and_compaction() {
        let reasoning = json!({"type":"reasoning","id":"rs","encrypted_content":"complete-secret","summary":[]});
        let compact = json!({"type":"compaction","id":"cp","encrypted_content":"summary-secret"});
        let tool = json!({"type":"function_call","call_id":"call","name":"exec","arguments":"{\"cmd\":\"pwd\"}"});
        let mut native = NativeResponse::default();
        native.apply(&json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","encrypted_content":"partial"}})).unwrap();
        for (n, item) in [reasoning.clone(), compact.clone(), tool.clone()]
            .iter()
            .enumerate()
        {
            native
                .apply(&json!({"type":"response.output_item.done","output_index":n,"item":item}))
                .unwrap();
        }
        native.apply(&json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[]}})).unwrap();
        let response = native.finish().unwrap();
        let mut input = json!([]);
        continue_with(
            "responses",
            &mut input,
            &response,
            &[("call".into(), json!({"out":"ok"}))],
        )
        .unwrap();
        assert_eq!(input[0], reasoning);
        assert_eq!(input[1], compact);
        assert_eq!(input[2], tool);
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(response["id"], "resp_1");
        assert!(NativeResponse::default().finish().is_err());
    }
}
