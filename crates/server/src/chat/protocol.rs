//! Provider-native continuation; UI Items are never used as model history.
use anyhow::{Context, ensure};
use ryzhub_proto::Citation;
use serde_json::{Value, json};

use crate::error::Code;

/// A summary the model wrote of its context, standing in for that context when the provider
/// cannot compact it (see `runner`).
pub const SUMMARY: &str = "ryzhub.summary";
/// Tool results shorter than this stay in the context: a placeholder would save little.
const OFFLOAD_MIN: usize = 1024;
/// How much of a moved result its placeholder shows.
const PREVIEW_CHARS: usize = 300;

/// Whether `response` replaces the context before it: a compaction (Responses, or Anthropic's
/// own, on demand or past its threshold) or a summary.
pub fn resets_context(response: &Value) -> bool {
    response["object"] == "response.compaction"
        || response["object"] == SUMMARY
        || response["content"]
            .as_array()
            .is_some_and(|content| content.iter().any(|b| b["type"] == "compaction"))
}

/// The context a summary leaves: the summary, and the model taking it up.
pub fn summary_context(summary: &str) -> Value {
    json!([
        {"role": "user", "content": format!("[Summary of the conversation so far, written when its context was compacted]\n\n{summary}")},
        {"role": "assistant", "content": "I have the summary of our conversation and will continue from it."},
    ])
}

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
    if response["object"] == SUMMARY {
        *messages = summary_context(response["summary"].as_str().unwrap_or_default());
        return Ok(());
    }
    // An on-demand compaction leads the context; what it summarized must go.
    if protocol != "responses" && response["stop_reason"] == "compaction" {
        let content = response["content"]
            .as_array()
            .context("INVALID_CHECKPOINT")?;
        ensure!(
            content.iter().any(|b| b["type"] == "compaction"),
            "INVALID_CHECKPOINT"
        );
        *messages = json!([{"role": "assistant", "content": content}]);
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
                json!({"type":"function_call_output","call_id":id,"output":output(protocol, result)}),
            );
        }
    } else {
        let blocks = response["content"]
            .as_array()
            .context("INVALID_CHECKPOINT")?;
        let resolved = |b: &Value| {
            results
                .iter()
                .any(|(id, _)| Some(id.as_str()) == b["id"].as_str())
        };
        let omitted = blocks
            .iter()
            .any(|b| b["type"] == "tool_use" && !resolved(b));
        // Without every client result the turn cannot resume, so a server tool call deferred
        // behind those calls is dropped with them.
        let deferred = if omitted {
            unresolved_server_calls(blocks)
        } else {
            vec![]
        };
        let content: Vec<_> = blocks
            .iter()
            .filter(|b| b["type"] != "tool_use" || resolved(b))
            .filter(|b| !b["id"].as_str().is_some_and(|id| deferred.contains(&id)))
            .cloned()
            .collect();
        match target.last_mut() {
            // A continued turn (`pause_turn`) extends the assistant message it resumes.
            Some(last) if last["role"] == "assistant" && last["content"].is_array() => {
                last["content"]
                    .as_array_mut()
                    .context("INVALID_CHECKPOINT")?
                    .extend(content);
            }
            _ if !content.is_empty() => {
                target.push(json!({"role":"assistant","content":content}));
            }
            _ => {}
        }
        if !results.is_empty() {
            let results: Vec<Value> = results
                .iter()
                .map(|(id, result)| {
                    json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "is_error": result.get("error").is_some(),
                        "content": output(protocol, result),
                    })
                })
                .collect();
            target.push(json!({"role": "user", "content": results}));
        }
    }
    Ok(())
}

/// Move the oldest tool results out of `messages` while it is estimated above `budget` tokens
/// (about four bytes each), until it is under 60% of it. Each leaves a placeholder naming its
/// call, which `read_tool_output` reads back. The oldest go first and placeholders are
/// deterministic, so the context's prefix (and the provider's cache of it) changes only as
/// results move. Results of the current turn, after the last user input, stay. Returns how
/// many moved.
pub fn offload(protocol: &str, messages: &mut Value, budget: usize) -> usize {
    let total = messages.to_string().len();
    if budget == 0 || total / 4 <= budget {
        return 0;
    }
    let mut excess = total.saturating_sub(budget * 4 * 6 / 10);
    let Some(list) = messages.as_array_mut() else {
        return 0;
    };
    let current = list
        .iter()
        .rposition(|m| user_input(protocol, m))
        .unwrap_or(0);
    let mut moved = 0;
    for message in &mut list[..current] {
        let slots: Vec<(String, &mut Value)> = if protocol == "responses" {
            match message["call_id"].as_str().map(str::to_owned) {
                Some(id) if message["type"] == "function_call_output" => {
                    vec![(id, &mut message["output"])]
                }
                _ => vec![],
            }
        } else if message["role"] == "user" {
            match message["content"].as_array_mut() {
                Some(blocks) => blocks
                    .iter_mut()
                    .filter(|b| b["type"] == "tool_result")
                    .filter_map(|b| {
                        let id = b["tool_use_id"].as_str()?.to_owned();
                        Some((id, &mut b["content"]))
                    })
                    .collect(),
                None => vec![],
            }
        } else {
            vec![]
        };
        for (id, slot) in slots {
            if excess == 0 {
                return moved;
            }
            let Some(text) = slot.as_str() else {
                continue;
            };
            if text.len() < OFFLOAD_MIN {
                continue;
            }
            let preview: String = text.chars().take(PREVIEW_CHARS).collect();
            let placeholder = format!(
                "[Moved out of the context to save space: this tool result ({} bytes) begins {preview:?}. Call read_tool_output with id {id:?} to read it.]",
                text.len()
            );
            excess = excess.saturating_sub(text.len().saturating_sub(placeholder.len()));
            *slot = Value::String(placeholder);
            moved += 1;
        }
    }
    moved
}

/// Whether a context entry is input from the user, rather than tool results.
fn user_input(protocol: &str, message: &Value) -> bool {
    if message["role"] != "user" {
        return false;
    }
    protocol == "responses"
        || message["content"].is_string()
        || message["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|b| b["type"] != "tool_result"))
}

/// A tool result as model context: its JSON text, or an image reference with that text.
fn output(protocol: &str, result: &Value) -> Value {
    super::media::tool_content(protocol, result).unwrap_or_else(|| result.to_string().into())
}

/// Ids of server tool calls in an assistant message's content that have no result block yet.
fn unresolved_server_calls(content: &[Value]) -> Vec<&str> {
    content
        .iter()
        .filter(|b| b["type"] == "server_tool_use" || b["type"] == "mcp_tool_use")
        .filter_map(|b| b["id"].as_str())
        .filter(|id| {
            !content
                .iter()
                .any(|r| r["tool_use_id"].as_str() == Some(id))
        })
        .collect()
}

/// Whether the provider left the assistant turn open: Anthropic paused its server-side loop
/// (`pause_turn`), or deferred a server tool call behind client tool calls. An open turn is
/// continued by re-sending it, or with only the client results; a user message would end it
/// and fails the request while a server call is unresolved.
pub fn turn_open(protocol: &str, response: &Value) -> bool {
    protocol != "responses"
        && (response["stop_reason"] == "pause_turn"
            || response["content"]
                .as_array()
                .is_some_and(|content| !unresolved_server_calls(content).is_empty()))
}

/// Drop unresolved server tool calls from the last assistant message so that a user message
/// can follow the context.
pub fn close_turn(protocol: &str, messages: &mut Value) {
    if protocol == "responses" {
        return;
    }
    let Some(list) = messages.as_array_mut() else {
        return;
    };
    let Some(index) = list.iter().rposition(|m| m["role"] == "assistant") else {
        return;
    };
    let Some(content) = list[index]["content"].as_array() else {
        return;
    };
    let open = unresolved_server_calls(content);
    if open.is_empty() {
        return;
    }
    let kept: Vec<Value> = content
        .iter()
        .filter(|b| !b["id"].as_str().is_some_and(|id| open.contains(&id)))
        .cloned()
        .collect();
    if kept.is_empty() {
        list.remove(index);
    } else {
        list[index]["content"] = Value::Array(kept);
    }
}

/// A response as shown: its text split around the provider's web searches.
pub struct Reply {
    /// The text before, between and after the searches, each with the pages it cites.
    pub texts: Vec<(String, Vec<Citation>)>,
    /// Provider ids of the searches, in order; one fewer than `texts`.
    pub searches: Vec<String>,
}

pub fn reply(protocol: &str, response: &Value) -> Reply {
    let mut reply = Reply {
        texts: vec![Default::default()],
        searches: vec![],
    };
    let split = |reply: &mut Reply, id: &Value| {
        reply.searches.push(id.as_str().unwrap_or_default().into());
        reply.texts.push(Default::default());
    };
    if protocol == "responses" {
        for item in each(&response["output"]) {
            if item["type"] == "web_search_call" {
                split(&mut reply, &item["id"]);
            }
            if item["type"] != "message" {
                continue;
            }
            let (text, citations) = reply.texts.last_mut().unwrap();
            for part in each(&item["content"]).filter(|c| c["type"] == "output_text") {
                text.push_str(part["text"].as_str().unwrap_or_default());
                for annotation in each(&part["annotations"]) {
                    if annotation["type"] == "url_citation" {
                        cite(citations, annotation, "");
                    }
                }
            }
        }
    } else {
        for block in each(&response["content"]) {
            if block["type"] == "server_tool_use" && block["name"] == "web_search" {
                split(&mut reply, &block["id"]);
            }
            if block["type"] != "text" {
                continue;
            }
            let (text, citations) = reply.texts.last_mut().unwrap();
            text.push_str(block["text"].as_str().unwrap_or_default());
            for citation in each(&block["citations"]) {
                let cited_text = citation["cited_text"].as_str().unwrap_or_default();
                cite(citations, citation, cited_text);
            }
            text.push_str(&source_links(block));
        }
    }
    reply
}

/// The pages an Anthropic text block cites.
pub fn block_citations(block: &Value) -> Vec<Citation> {
    let mut cited = vec![];
    for citation in each(&block["citations"]) {
        cite(
            &mut cited,
            citation,
            citation["cited_text"].as_str().unwrap_or_default(),
        );
    }
    cited
}

/// The page a Responses stream event newly cites.
pub fn annotation_citations(event: &Value) -> Vec<Citation> {
    let mut cited = vec![];
    let annotation = &event["annotation"];
    if event["type"] == "response.output_text.annotation.added"
        && annotation["type"] == "url_citation"
    {
        cite(&mut cited, annotation, "");
    }
    cited
}

/// The sources an Anthropic text block cites, as links to follow its text the way Responses
/// models write them: ` ([site](url))`. Streamed when the block ends, and part of its reply.
pub fn source_links(block: &Value) -> String {
    block_citations(block)
        .iter()
        .map(|citation| {
            let site = reqwest::Url::parse(&citation.url)
                .ok()
                .and_then(|url| {
                    url.host_str()
                        .map(|h| h.trim_start_matches("www.").to_owned())
                })
                .unwrap_or_default();
            format!(" ([{site}]({}))", citation.url)
        })
        .collect()
}

/// Add a cited page once, in order of first citation.
fn cite(found: &mut Vec<Citation>, citation: &Value, cited_text: &str) {
    let Some(url) = citation["url"].as_str() else {
        return;
    };
    // Only links a client may open; the URL comes from the provider's search results.
    if !(url.starts_with("https://") || url.starts_with("http://"))
        || found.iter().any(|c| c.url == url)
    {
        return;
    }
    found.push(Citation {
        url: url.into(),
        title: citation["title"].as_str().unwrap_or_default().into(),
        cited_text: cited_text.into(),
    });
}

fn each(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}

/// A provider web search seen in a response stream.
#[derive(Debug, PartialEq)]
pub enum Search {
    /// The search began; text after it belongs to a new item. `arguments` is `{"query"}` or,
    /// for a page the search opened, `{"url"}`.
    Started { id: String, arguments: Value },
    /// The search returned, possibly one an earlier step began: `{"matches"}` or `{"error"}`.
    Done {
        id: String,
        arguments: Option<Value>,
        result: Value,
    },
}

/// Follows the provider's web searches through a response stream.
#[derive(Default)]
pub struct SearchWatch {
    /// Anthropic search calls by block index: id, input from the start event, streamed input.
    calls: std::collections::HashMap<u64, (String, Value, String)>,
}

impl SearchWatch {
    pub fn apply(&mut self, protocol: &str, event: &Value) -> Option<Search> {
        if protocol == "responses" {
            let item = &event["item"];
            if item["type"] != "web_search_call" {
                return None;
            }
            let id = item["id"].as_str()?.to_owned();
            let arguments = search_arguments(&item["action"]);
            return match event["type"].as_str()? {
                "response.output_item.added" => Some(Search::Started { id, arguments }),
                "response.output_item.done" => Some(Search::Done {
                    id,
                    arguments: Some(arguments),
                    result: match item["action"]["sources"].as_array() {
                        Some(sources) => json!({ "matches": sources
                            .iter()
                            .map(|s| json!({"url": s["url"], "title": s["title"]}))
                            .collect::<Vec<_>>() }),
                        None => json!({}),
                    },
                }),
                _ => None,
            };
        }
        let index = event["index"].as_u64().unwrap_or(0);
        match event["type"].as_str()? {
            "content_block_start" => {
                let block = &event["content_block"];
                if block["type"] == "server_tool_use" && block["name"] == "web_search" {
                    let id = block["id"].as_str()?.to_owned();
                    self.calls
                        .insert(index, (id, block["input"].clone(), String::new()));
                } else if block["type"] == "web_search_tool_result" {
                    let content = &block["content"];
                    return Some(Search::Done {
                        id: block["tool_use_id"].as_str()?.to_owned(),
                        arguments: None,
                        result: match content.as_array() {
                            Some(results) => json!({ "matches": results
                                .iter()
                                .map(|r| json!({"url": r["url"], "title": r["title"]}))
                                .collect::<Vec<_>>() }),
                            None => json!({ "error": content["error_code"] }),
                        },
                    });
                }
                None
            }
            "content_block_delta" => {
                if let Some((_, _, input)) = self.calls.get_mut(&index) {
                    input.push_str(event["delta"]["partial_json"].as_str().unwrap_or_default());
                }
                None
            }
            "content_block_stop" => {
                let (id, start, input) = self.calls.remove(&index)?;
                let input = serde_json::from_str(&input).unwrap_or(start);
                Some(Search::Started {
                    id,
                    arguments: search_arguments(&input),
                })
            }
            _ => None,
        }
    }
}

/// What a search looked for: its query, or the page it opened.
fn search_arguments(action: &Value) -> Value {
    let query = action["query"]
        .as_str()
        .or_else(|| action["queries"][0].as_str())
        .filter(|q| !q.is_empty());
    match (query, action["url"].as_str()) {
        (Some(query), _) => json!({ "query": query }),
        (None, Some(url)) => json!({ "url": url }),
        (None, None) => json!({}),
    }
}

/// The client tool calls in a response. Fails on response items the loop cannot continue.
pub fn tool_calls(protocol: &str, response: &Value) -> anyhow::Result<Vec<Value>> {
    if protocol == "responses" {
        let output = response["output"]
            .as_array()
            .context("INVALID_MODEL_RESPONSE")?;
        let mut tools = vec![];
        for item in output {
            match item["type"].as_str().unwrap_or("") {
                "function_call" => {
                    let args: Value = serde_json::from_str(
                        item["arguments"].as_str().context("INVALID_TOOL_CALL")?,
                    )?;
                    tools.push(json!({"id":item["call_id"],"name":item["name"],"input":args}));
                }
                // The provider ran the search; its sources are in the message annotations.
                "message" | "reasoning" | "compaction" | "web_search_call" => {}
                _ => anyhow::bail!("UNSUPPORTED_RESPONSE_ITEM"),
            }
        }
        Ok(tools)
    } else {
        let content = response["content"]
            .as_array()
            .context("INVALID_MODEL_RESPONSE")?;
        Ok(content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .cloned()
            .collect())
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
    fn old_tool_results_move_out_of_a_long_context_oldest_first() {
        let result = |id: &str, size: usize| json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"x".repeat(size)}]});
        let call = |id: &str| json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":"exec","input":{}}]});
        let mut messages = json!([
            {"role":"user","content":"first"},
            call("a"), result("a", 6000),
            call("b"), result("b", 6000),
            call("small"), result("small", 100),
            {"role":"user","content":"now"},
            call("c"), result("c", 6000),
        ]);
        let before = messages.clone();
        // Within budget: nothing moves.
        assert_eq!(offload("anthropic", &mut messages, 10_000), 0);
        assert_eq!(messages, before);
        // Over 3000 tokens: the oldest go until under 1800; the current turn's stays.
        assert_eq!(offload("anthropic", &mut messages, 3000), 2);
        let content = |i: usize| {
            messages[i]["content"][0]["content"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert!(
            content(2).starts_with("[Moved out of the context") && content(2).contains("\"a\"")
        );
        assert!(content(4).contains("read_tool_output"));
        assert_eq!(content(6).len(), 100);
        assert_eq!(content(9).len(), 6000);
        // The same context moves the same results the same way.
        let mut again = before.clone();
        offload("anthropic", &mut again, 3000);
        assert_eq!(again, messages);

        let mut items = json!([
            {"role":"user","content":"first"},
            {"type":"function_call","call_id":"a","name":"exec","arguments":"{}"},
            {"type":"function_call_output","call_id":"a","output":"y".repeat(8000)},
            {"role":"user","content":"now"},
        ]);
        assert_eq!(offload("responses", &mut items, 1000), 1);
        assert!(items[2]["output"].as_str().unwrap().contains("\"a\""));
    }

    #[test]
    fn compactions_and_summaries_replace_the_context() {
        let mut messages = json!([{"role":"user","content":"long ago"},{"role":"assistant","content":[{"type":"text","text":"old"}]}]);
        // Anthropic on demand: its block leads what follows.
        let compaction = json!({"content":[{"type":"compaction","content":"what happened"}],"stop_reason":"compaction"});
        assert!(resets_context(&compaction));
        continue_with("anthropic", &mut messages, &compaction, &[]).unwrap();
        assert_eq!(
            messages,
            json!([{"role":"assistant","content":[{"type":"compaction","content":"what happened"}]}])
        );
        // Past the threshold mid-reply: the block stays where it came, ahead of the reply.
        let mut messages = json!([{"role":"user","content":"hi"}]);
        let threshold = json!({"content":[{"type":"compaction","content":"s"},{"type":"text","text":"answer"}],"stop_reason":"end_turn"});
        assert!(resets_context(&threshold));
        continue_with("anthropic", &mut messages, &threshold, &[]).unwrap();
        assert_eq!(messages[1]["content"][0]["type"], "compaction");
        assert_eq!(messages.as_array().unwrap().len(), 2);
        // A summary, in either protocol.
        let summary = json!({"object":SUMMARY,"summary":"the gist","content":[],"output":[]});
        assert!(resets_context(&summary));
        for protocol in ["anthropic", "responses"] {
            let mut messages = json!([{"role":"user","content":"long ago"}]);
            continue_with(protocol, &mut messages, &summary, &[]).unwrap();
            assert!(
                messages[0]["content"]
                    .as_str()
                    .unwrap()
                    .ends_with("the gist")
            );
            assert_eq!(messages[1]["role"], "assistant");
            assert!(tool_calls(protocol, &summary).unwrap().is_empty());
        }
        assert!(!resets_context(
            &json!({"content":[{"type":"text","text":"plain"}]})
        ));
    }

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

    fn search(id: &str) -> [Value; 2] {
        [
            json!({"type":"server_tool_use","id":id,"name":"web_search","input":{"query":"q"}}),
            json!({"type":"web_search_tool_result","tool_use_id":id,"content":[
                {"type":"web_search_result","url":"https://a.example","title":"A","encrypted_content":"secret"}
            ]}),
        ]
    }

    #[test]
    fn paused_turn_stays_open_and_resumes_into_one_assistant_message() {
        let [call, result] = search("srvtoolu_1");
        let paused =
            json!({"stop_reason":"pause_turn","content":[{"type":"text","text":"先搜索"}, call]});
        assert!(turn_open("anthropic", &paused));
        let mut messages = json!([{"role":"user","content":"问题"}]);
        continue_with("anthropic", &mut messages, &paused, &[]).unwrap();
        assert_eq!(messages.as_array().unwrap().len(), 2);
        assert_eq!(messages[1]["content"][1], call);
        let resumed = json!({"stop_reason":"end_turn","content":[result.clone(),{"type":"text","text":"答案"}]});
        assert!(!turn_open("anthropic", &resumed));
        continue_with("anthropic", &mut messages, &resumed, &[]).unwrap();
        assert_eq!(messages.as_array().unwrap().len(), 2);
        assert_eq!(messages[1]["content"].as_array().unwrap().len(), 4);
        assert_eq!(messages[1]["content"][2], result);
        // A completed search and a pause outside the Anthropic protocol leave nothing open.
        let [call, result] = search("srvtoolu_2");
        assert!(!turn_open(
            "anthropic",
            &json!({"stop_reason":"end_turn","content":[call, result]})
        ));
        assert!(!turn_open(
            "responses",
            &json!({"stop_reason":"pause_turn"})
        ));
    }

    #[test]
    fn server_call_deferred_behind_client_tools_is_kept_only_with_their_results() {
        let [call, _] = search("srvtoolu_1");
        let tool = json!({"type":"tool_use","id":"toolu_1","name":"exec","input":{}});
        let response = json!({"stop_reason":"tool_use","content":[call.clone(), tool.clone()]});
        assert!(turn_open("anthropic", &response));
        let mut messages = json!([]);
        let results = [("toolu_1".to_owned(), json!({"out":"ok"}))];
        continue_with("anthropic", &mut messages, &response, &results).unwrap();
        assert_eq!(messages[0]["content"], json!([call, tool]));
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages.as_array().unwrap().len(), 2);
        // A fork without the client result cannot resume the turn, so the search goes too.
        let mut fork = json!([]);
        continue_with("anthropic", &mut fork, &response, &[]).unwrap();
        assert_eq!(fork, json!([]));
    }

    #[test]
    fn closing_a_turn_drops_unresolved_server_calls_before_a_user_message() {
        let [call, _] = search("srvtoolu_1");
        let [done, result] = search("srvtoolu_2");
        let tool = json!({"type":"tool_use","id":"toolu_1","name":"exec","input":{}});
        let mut messages = json!([
            {"role":"user","content":"问题"},
            {"role":"assistant","content":[done.clone(), result.clone(), call, tool.clone()]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}
        ]);
        close_turn("anthropic", &mut messages);
        assert_eq!(messages[1]["content"], json!([done, result, tool]));
        let [call, _] = search("srvtoolu_3");
        let mut only =
            json!([{"role":"user","content":"问题"},{"role":"assistant","content":[call]}]);
        close_turn("anthropic", &mut only);
        assert_eq!(only, json!([{"role":"user","content":"问题"}]));
    }

    #[test]
    fn a_reply_splits_at_searches_and_keeps_web_citations_per_text() {
        let [call, result] = search("srvtoolu_1");
        let anthropic = json!({"content":[
            {"type":"thinking","thinking":"","signature":"s"},
            {"type":"text","text":"先搜索"},
            call, result,
            {"type":"text","text":"a","citations":[
                {"type":"web_search_result_location","url":"https://b.example","title":"B","cited_text":"b"},
                {"type":"char_location","document_index":0,"cited_text":"doc"}
            ]},
            {"type":"text","text":"b","citations":[
                {"type":"web_search_result_location","url":"https://a.example","title":"A","cited_text":"a"},
                {"type":"web_search_result_location","url":"https://b.example","title":"B","cited_text":"again"},
                {"type":"web_search_result_location","url":"javascript:alert(1)","title":"X"}
            ]}
        ]});
        let reply = reply("anthropic", &anthropic);
        assert_eq!(reply.searches, ["srvtoolu_1"]);
        assert_eq!(reply.texts[0], ("先搜索".into(), vec![]));
        assert_eq!(
            reply.texts[1].0,
            "a ([b.example](https://b.example))b ([a.example](https://a.example)) ([b.example](https://b.example))"
        );
        assert_eq!(
            reply.texts[1]
                .1
                .iter()
                .map(|c| (c.url.as_str(), c.cited_text.as_str()))
                .collect::<Vec<_>>(),
            [("https://b.example", "b"), ("https://a.example", "a")]
        );
        let responses = json!({"output":[
            {"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"q"}},
            {"type":"message","content":[{"type":"output_text","text":"答案 ([a](https://a.example))","annotations":[
                {"type":"url_citation","url":"https://a.example","title":"A","start_index":3,"end_index":24}
            ]}]}
        ]});
        let reply = super::reply("responses", &responses);
        assert_eq!(reply.searches, ["ws_1"]);
        assert_eq!(reply.texts[0].0, "");
        assert_eq!(reply.texts[1].0, "答案 ([a](https://a.example))");
        assert_eq!(
            (
                reply.texts[1].1[0].url.as_str(),
                reply.texts[1].1[0].title.as_str()
            ),
            ("https://a.example", "A")
        );
        assert!(tool_calls("responses", &responses).unwrap().is_empty());
        assert!(
            tool_calls(
                "responses",
                &json!({"output":[{"type":"image_generation_call"}]})
            )
            .is_err()
        );
        let mut input = json!([]);
        continue_with("responses", &mut input, &responses, &[]).unwrap();
        assert_eq!(input[0]["type"], "web_search_call");
    }

    #[test]
    fn citations_stream_with_annotations_and_cited_blocks() {
        let added = json!({"type":"response.output_text.annotation.added","item_id":"msg_1",
            "output_index":1,"content_index":0,"annotation_index":0,
            "annotation":{"type":"url_citation","url":"https://a.example","title":"A","start_index":3,"end_index":24}});
        let found = annotation_citations(&added);
        assert_eq!(
            (found[0].url.as_str(), found[0].title.as_str()),
            ("https://a.example", "A")
        );
        assert!(
            annotation_citations(&json!({"type":"response.output_text.delta","delta":"x"}))
                .is_empty()
        );
        let block = json!({"type":"text","text":"claim","citations":[
            {"type":"web_search_result_location","url":"https://www.b.example/p","title":"B","cited_text":"b"},
            {"type":"web_search_result_location","url":"https://www.b.example/p","title":"B","cited_text":"again"}
        ]});
        assert_eq!(block_citations(&block)[0].cited_text, "b");
        assert_eq!(
            source_links(&block),
            " ([b.example](https://www.b.example/p))"
        );
        assert_eq!(source_links(&json!({"type":"text","text":"plain"})), "");
    }

    #[test]
    fn searches_are_followed_through_both_streams() {
        let mut watch = SearchWatch::default();
        let anthropic = [
            json!({"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"rust\"}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[
                {"type":"web_search_result","url":"https://a.example","title":"A","encrypted_content":"secret"}
            ]}}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_0","content":
                {"type":"web_search_tool_result_error","error_code":"max_uses_exceeded"}
            }}),
            json!({"type":"content_block_start","index":4,"content_block":{"type":"server_tool_use","id":"srvtoolu_2","name":"code_execution","input":{}}}),
            json!({"type":"content_block_stop","index":4}),
        ];
        let seen: Vec<_> = anthropic
            .iter()
            .filter_map(|e| watch.apply("anthropic", e))
            .collect();
        assert_eq!(
            seen,
            [
                Search::Started {
                    id: "srvtoolu_1".into(),
                    arguments: json!({"query":"rust"})
                },
                Search::Done {
                    id: "srvtoolu_1".into(),
                    arguments: None,
                    result: json!({"matches":[{"url":"https://a.example","title":"A"}]})
                },
                Search::Done {
                    id: "srvtoolu_0".into(),
                    arguments: None,
                    result: json!({"error":"max_uses_exceeded"})
                },
            ]
        );
        let responses = [
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress"}}),
            json!({"type":"response.web_search_call.searching","output_index":0,"item_id":"ws_1"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"completed",
                "action":{"type":"search","query":"rust","sources":[{"type":"url","url":"https://a.example"}]}}}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"web_search_call","id":"ws_2","status":"completed",
                "action":{"type":"open_page","url":"https://b.example"}}}),
        ];
        let seen: Vec<_> = responses
            .iter()
            .filter_map(|e| watch.apply("responses", e))
            .collect();
        assert_eq!(
            seen,
            [
                Search::Started {
                    id: "ws_1".into(),
                    arguments: json!({})
                },
                Search::Done {
                    id: "ws_1".into(),
                    arguments: Some(json!({"query":"rust"})),
                    result: json!({"matches":[{"url":"https://a.example","title":null}]})
                },
                Search::Done {
                    id: "ws_2".into(),
                    arguments: Some(json!({"url":"https://b.example"})),
                    result: json!({})
                },
            ]
        );
    }
}
