//! Anthropic's native SSE protocol. Preserve opaque blocks and signatures in checkpoints.
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::error::Code;

const MAX_EVENT: usize = 4 * 1024 * 1024;

/// Splits a server-sent event stream into the JSON payloads of its `data:` fields.
#[derive(Default)]
pub struct SseDecoder {
    pending: Vec<u8>,
    /// Bytes of `pending` already known to hold no newline.
    scanned: usize,
    data: Vec<String>,
    event_bytes: usize,
}

impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> anyhow::Result<Vec<Value>> {
        self.pending.extend_from_slice(bytes);
        ensure!(self.pending.len() <= MAX_EVENT, "SSE_EVENT_TOO_LARGE");
        let mut events = Vec::new();
        while let Some(offset) = self.pending[self.scanned..]
            .iter()
            .position(|b| *b == b'\n')
        {
            let raw: Vec<_> = self.pending.drain(..=self.scanned + offset).collect();
            self.scanned = 0;
            let line = std::str::from_utf8(&raw)?.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    let joined = self.data.join("\n");
                    self.data.clear();
                    self.event_bytes = 0;
                    if joined != "[DONE]" {
                        events.push(serde_json::from_str(&joined)?);
                    }
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                self.event_bytes += data.len();
                ensure!(self.event_bytes <= MAX_EVENT, "SSE_EVENT_TOO_LARGE");
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
            }
        }
        self.scanned = self.pending.len();
        Ok(events)
    }
}

/// Reassembles a streamed Anthropic message, including blocks the UI never shows.
#[derive(Default)]
pub struct NativeMessage {
    response: Value,
    stopped: bool,
    blocks: BTreeMap<usize, Value>,
    json_fragments: BTreeMap<usize, String>,
}

impl NativeMessage {
    /// Returns visible text only; thinking and opaque data never become UI text.
    pub fn apply(&mut self, event: &Value) -> anyhow::Result<Option<String>> {
        let index = event["index"].as_u64().unwrap_or(0) as usize;
        ensure!(index < 1024, "TOO_MANY_CONTENT_BLOCKS");
        match event["type"].as_str().unwrap_or("") {
            "message_start" => {
                self.response = event["message"].clone();
                ensure!(self.response.is_object(), "INVALID_MESSAGE_START");
            }
            "content_block_start" => {
                self.blocks.insert(index, event["content_block"].clone());
            }
            "content_block_delta" => {
                let block = self
                    .blocks
                    .get_mut(&index)
                    .context("MISSING_CONTENT_BLOCK")?;
                let delta = &event["delta"];
                let (field, value) = match delta["type"].as_str().unwrap_or("") {
                    "text_delta" => ("text", "text"),
                    "thinking_delta" => ("thinking", "thinking"),
                    "signature_delta" => ("signature", "signature"),
                    "input_json_delta" => {
                        let fragments = self.json_fragments.entry(index).or_default();
                        fragments.push_str(delta["partial_json"].as_str().unwrap_or(""));
                        ensure!(fragments.len() <= MAX_EVENT, "TOOL_INPUT_TOO_LARGE");
                        return Ok(None);
                    }
                    // A context compaction's summary, whole in one delta; never shown.
                    "compaction_delta" => {
                        let summary = delta["content"]
                            .as_str()
                            .or_else(|| delta["text"].as_str())
                            .context("INVALID_CONTENT_DELTA")?;
                        match &mut block["content"] {
                            Value::String(text) => text.push_str(summary),
                            other => *other = Value::String(summary.to_owned()),
                        }
                        return Ok(None);
                    }
                    "citations_delta" => {
                        if block.get("citations").is_none() {
                            block["citations"] = json!([]);
                        }
                        block["citations"]
                            .as_array_mut()
                            .context("INVALID_CITATIONS")?
                            .push(delta["citation"].clone());
                        return Ok(None);
                    }
                    _ => {
                        // Reject an unknown incremental encoding rather than corrupt a checkpoint.
                        anyhow::bail!("UNSUPPORTED_CONTENT_DELTA");
                    }
                };
                let chunk = delta[value].as_str().context("INVALID_CONTENT_DELTA")?;
                // Appended in place: rebuilding the string per delta is quadratic in its length.
                match &mut block[field] {
                    Value::String(text) => text.push_str(chunk),
                    other => *other = Value::String(chunk.to_owned()),
                }
                let length = block[field].as_str().map_or(0, str::len);
                ensure!(length <= 16 * MAX_EVENT, Code::ContentTooLarge);
                if field == "text" {
                    return Ok(Some(chunk.to_owned()));
                }
            }
            "content_block_stop" => {
                if let Some(input) = self.json_fragments.remove(&index) {
                    self.blocks
                        .get_mut(&index)
                        .context("MISSING_CONTENT_BLOCK")?["input"] = serde_json::from_str(&input)?;
                }
            }
            "message_delta" => {
                if let Some(delta) = event["delta"].as_object() {
                    let response = self
                        .response
                        .as_object_mut()
                        .context("MISSING_MESSAGE_START")?;
                    for (k, v) in delta {
                        response.insert(k.clone(), v.clone());
                    }
                }
                if let Some(usage) = event["usage"].as_object() {
                    if !self.response["usage"].is_object() {
                        self.response["usage"] = json!({});
                    }
                    for (k, v) in usage {
                        self.response["usage"][k] = v.clone();
                    }
                }
            }
            "message_stop" => {
                ensure!(
                    self.response.is_object() && self.json_fragments.is_empty(),
                    "INCOMPLETE_MESSAGE"
                );
                self.response["content"] = Value::Array(self.blocks.values().cloned().collect());
                self.stopped = true;
            }
            "error" => anyhow::bail!(Code::UpstreamStreamError),
            _ => {} // ping and future metadata events pass through the gateway.
        }
        Ok(None)
    }

    /// A content block as received so far.
    pub fn block(&self, index: usize) -> Option<&Value> {
        self.blocks.get(&index)
    }

    pub fn finish(self) -> anyhow::Result<Value> {
        ensure!(self.stopped, Code::UpstreamStreamInterrupted);
        Ok(self.response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_compaction_block_streams_whole_and_shows_nothing() {
        let mut message = NativeMessage::default();
        let events = [
            json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[]}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"compaction","content":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"compaction_delta","content":"summary of before"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"reply"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
            json!({"type":"message_stop"}),
        ];
        let shown: Vec<String> = events
            .iter()
            .filter_map(|e| message.apply(e).unwrap())
            .collect();
        assert_eq!(shown, ["reply"]);
        let response = message.finish().unwrap();
        assert_eq!(
            response["content"][0],
            json!({"type":"compaction","content":"summary of before"})
        );
    }

    #[test]
    fn fragmented_unicode_sse_preserves_native_thinking_tools_and_cumulative_usage() {
        let events = vec![
            json!({"type":"message_start","message":{"id":"msg","role":"assistant","content":[],"usage":{"input_tokens":20,"cache_read_input_tokens":12,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"private"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque-signature"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"你好🦀"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"call","name":"exec","input":{}}}),
            json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":"}}),
            json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"\"pwd\"}"}}),
            json!({"type":"content_block_stop","index":3}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
            json!({"type":"message_delta","delta":{},"usage":{"output_tokens":8}}),
            json!({"type":"message_stop"}),
        ];
        let wire = events
            .iter()
            .map(|e| {
                format!(
                    ": ping\r\nevent: {}\r\ndata: {e}\r\n\r\n",
                    e["type"].as_str().unwrap()
                )
            })
            .collect::<String>();
        let mut decoder = SseDecoder::default();
        let mut message = NativeMessage::default();
        let mut visible = String::new();
        for byte in wire.as_bytes() {
            for event in decoder.push(&[*byte]).unwrap() {
                if let Some(text) = message.apply(&event).unwrap() {
                    visible.push_str(&text);
                }
            }
        }
        let response = message.finish().unwrap();
        assert_eq!(visible, "你好🦀");
        assert_eq!(response["content"][0]["signature"], "opaque-signature");
        assert_eq!(response["content"][1]["data"], "opaque");
        assert_eq!(response["content"][3]["input"]["cmd"], "pwd");
        assert_eq!(response["usage"]["output_tokens"], 8);
        assert_eq!(response["usage"]["cache_read_input_tokens"], 12);
    }

    #[test]
    fn interrupted_or_oversized_stream_cannot_become_a_completed_checkpoint() {
        assert!(NativeMessage::default().finish().is_err());
        assert!(
            SseDecoder::default()
                .push(&vec![b'x'; MAX_EVENT + 1])
                .is_err()
        );
    }
}
