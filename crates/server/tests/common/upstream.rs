//! The model providers' side, scripted by keywords in the last prompt: Anthropic Messages for
//! the Chat engine, Claude Code and OpenCode, OpenAI Responses for Codex, an MCP server and an
//! embeddings endpoint.
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

/// Every request the upstreams received, with its headers.
pub type Requests = Arc<Mutex<Vec<(Value, HeaderMap)>>>;

/// A 1×1 PNG, small enough to reach the model unchanged.
pub const DOT_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

/// A reply handing over a file of the workspace.
const SHARED_REPLY: &str = "这是报告：[报告](sandbox:/workspace/share/report.txt)";

pub fn router(requests: Requests) -> Router {
    Router::new()
        .route("/v1/messages", post(messages))
        // Token counts are answered without being recorded.
        .route(
            "/v1/messages/count_tokens",
            post(|| async { Json(json!({"input_tokens":7})) }),
        )
        .route("/v1/responses", post(responses))
        .route("/v1/responses/compact", post(compact))
        .route("/v1/embeddings", post(embeddings))
        .route("/mcp", post(mcp))
        .with_state(requests)
}

/// `events` as server-sent events, each after `delay`.
fn stream(events: Vec<Value>, delay: Duration) -> Response {
    let events = futures_util::stream::unfold(events.into_iter(), move |mut events| async move {
        let event = events.next()?;
        tokio::time::sleep(delay).await;
        let kind = event["type"].as_str().unwrap();
        let frame = Bytes::from(format!("event: {kind}\ndata: {event}\n\n"));
        Some((Ok::<_, std::io::Error>(frame), events))
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(events))
        .unwrap()
}

fn text(text: &str) -> Value {
    json!({"type":"text","text":text})
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"type":"tool_use","id":id,"name":name,"input":input})
}

/// A sandbox command, as the Chat engine's `exec` tool takes it.
fn exec(cmd: &str, timeout_ms: u64) -> Value {
    tool_use(
        "call_exec",
        "exec",
        json!({"cmd":cmd,"timeout_ms":timeout_ms}),
    )
}

fn offers(body: &Value, tool: &str) -> bool {
    body["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|t| t["name"] == tool))
}

/// What an `exec` prompt runs: a call through the guest's model proxy for `proxyexec`.
fn command(prompt: &str) -> &'static str {
    if prompt.contains("proxyexec") {
        r#"sleep 2; wget -Y off -qO- --header 'Content-Type: application/json' --post-data '{"model":"fixture-model","max_tokens":32,"stream":true,"messages":[{"role":"user","content":"guest local proxy"}]}' http://127.0.0.1:8090/v1/messages"#
    } else {
        "printf once >> marker; sleep 1; printf '真实沙箱结果'"
    }
}

/// The answer when nothing in the prompt asks for more.
fn answer(body: &Value, result: bool) -> Value {
    text(if result {
        "工具结果已用于回答"
    } else if body.get("tools").is_none() {
        "只读旁问完成"
    } else {
        "本地协议联调：你好 Rust"
    })
}

async fn messages(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    requests.lock().unwrap().push((body.clone(), headers));
    let last = &body["messages"].as_array().unwrap().last().unwrap()["content"];
    // A message carrying files is a list of parts; its text parts make the prompt.
    let prompt = match last {
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.as_str().unwrap_or("").to_owned(),
    };
    let result = last
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"));
    // Compaction on demand: a summary in place of the context, unless it says "nocompact".
    if body.get("compaction").is_some() {
        if body["messages"].to_string().contains("nocompact") {
            let error = json!({"type":"error",
                "error":{"type":"invalid_request_error","message":"compaction unavailable"}});
            return (StatusCode::BAD_REQUEST, Json(error)).into_response();
        }
        return Json(json!({
            "id":"fixture-compaction","type":"message","role":"assistant","model":"fixture-model",
            "content":[{"type":"compaction","content":"压缩摘要"}],"stop_reason":"compaction",
            "usage":{"input_tokens":0,"output_tokens":0,
                "iterations":[{"type":"compaction","input_tokens":40,"output_tokens":6}]},
        }))
        .into_response();
    }
    // The summary a context falls back to.
    if body["stream"] != true && prompt.starts_with("Summarize our conversation") {
        return Json(json!({
            "id":"fixture-summary","type":"message","role":"assistant","model":"fixture-model",
            "content":[text("摘要内容")],"stop_reason":"end_turn",
            "usage":{"input_tokens":30,"output_tokens":4},
        }))
        .into_response();
    }
    let claude = offers(&body, "Bash");
    // OpenCode, over ACP: its tools have lower-case names.
    let opencode = offers(&body, "bash");
    let mut content = vec![
        json!({"type":"thinking","thinking":"opaque reasoning","signature":"signed-block"}),
        json!({"type":"redacted_thinking","data":"opaque-data"}),
    ];
    content.extend(if !result && prompt.contains("HARNESS_SHARE") {
        vec![text(SHARED_REPLY)]
    } else if !result && prompt.contains("HARNESS_SUBAGENT") {
        // A subagent Claude Code started.
        vec![text("子任务完成")]
    } else if claude || opencode {
        harness_reply(&body, &prompt, result, claude)
    } else {
        chat_reply(&body, &prompt, result)
    });
    let stop = if content.iter().any(|b| b["type"] == "server_tool_use") {
        "pause_turn"
    } else if content.iter().any(|b| b["type"] == "tool_use") {
        "tool_use"
    } else {
        "end_turn"
    };
    let id = format!("msg_{}", Uuid::now_v7());
    if body["stream"] != true {
        return Json(json!({
            "id":id,"type":"message","role":"assistant","model":body["model"],"content":content,
            "stop_reason":stop,"usage":{"input_tokens":25,"output_tokens":8},
        }))
        .into_response();
    }
    let usage = json!({"input_tokens":25,"output_tokens":1,"cache_read_input_tokens":12});
    let start = json!({"id":id,"type":"message","role":"assistant","content":[],
        "model":body["model"],"usage":usage});
    let mut events = vec![
        json!({"type":"message_start","message":start}),
        json!({"type":"ping"}),
    ];
    for (index, block) in content.iter().enumerate() {
        events.extend(block_events(index, block));
    }
    if prompt.contains("futuredelta") {
        // A delta of a kind the API adds later, in the last block before it stops.
        let delta = json!({"type":"content_block_delta","index":content.len() - 1,
            "delta":{"type":"future_delta","data":"opaque"}});
        events.insert(events.len() - 1, delta);
    }
    events.push(
        json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":4}}),
    );
    // A later usage-only delta; the AI SDK behind OpenCode reads it as an unknown finish
    // and continues the turn, so it gets the one delta the API sends.
    if !opencode {
        events.push(json!({"type":"message_delta","delta":{},"usage":{"output_tokens":8}}));
    }
    events.push(json!({"type":"message_stop"}));
    if prompt.contains("interrupt") {
        events.pop();
    }
    let delay = if prompt.contains("slow") { 100 } else { 5 };
    stream(events, Duration::from_millis(delay))
}

/// A content block as the API streams it: opened empty, then filled by deltas.
fn block_events(index: usize, block: &Value) -> Vec<Value> {
    let mut start = block.clone();
    let mut deltas = vec![];
    match block["type"].as_str().unwrap() {
        "text" => {
            start = text("");
            for citation in block["citations"].as_array().into_iter().flatten() {
                deltas.push(json!({"type":"citations_delta","citation":citation}));
            }
            deltas.push(json!({"type":"text_delta","text":block["text"]}));
        }
        "thinking" => {
            start["thinking"] = json!("");
            start["signature"] = json!("");
            deltas.push(json!({"type":"thinking_delta","thinking":block["thinking"]}));
            deltas.push(json!({"type":"signature_delta","signature":block["signature"]}));
        }
        "tool_use" | "server_tool_use" => {
            start["input"] = json!({});
            let input = block["input"].to_string();
            deltas.push(json!({"type":"input_json_delta","partial_json":input}));
        }
        "compaction" => {
            start["content"] = json!("");
            deltas.push(json!({"type":"compaction_delta","content":block["content"]}));
        }
        _ => {}
    }
    let mut events =
        vec![json!({"type":"content_block_start","index":index,"content_block":start})];
    for delta in deltas {
        events.push(json!({"type":"content_block_delta","index":index,"delta":delta}));
    }
    events.push(json!({"type":"content_block_stop","index":index}));
    events
}

/// What Claude Code's or OpenCode's model does, asked with `HARNESS_*` keywords.
fn harness_reply(body: &Value, prompt: &str, result: bool, claude: bool) -> Vec<Value> {
    let tools = body.get("tools").is_some();
    if !result && tools && prompt.contains("HARNESS_EXEC") {
        let id = format!("call_exec_{}", Uuid::now_v7().simple());
        let input = json!({"command":command(prompt),"description":"Local execution"});
        let bash = if claude { "Bash" } else { "bash" };
        vec![text("准备执行工具"), tool_use(&id, bash, input)]
    } else if !result && tools && claude && prompt.contains("HARNESS_QUESTION") {
        let option = |label: &str| json!({"label":label,"description":label});
        let question = json!({"question":"选择一种颜色","header":"颜色","multiSelect":false,
            "options":[option("Blue"), option("Green")]});
        let input = json!({"questions":[question]});
        vec![tool_use("call_question", "AskUserQuestion", input)]
    } else if !result && tools && !claude && prompt.contains("question") {
        let input = json!({"question":"选择一种颜色"});
        vec![tool_use("call_question", "ask_user", input)]
    } else if claude && !result && prompt.contains("HARNESS_AGENT") {
        // The SDK names its subagent tool Agent (Task in older releases).
        let agent = if offers(body, "Agent") {
            "Agent"
        } else {
            "Task"
        };
        let input = json!({"description":"Look around","subagent_type":"general-purpose",
            "prompt":"HARNESS_SUBAGENT list the workspace"});
        vec![tool_use("call_agent", agent, input)]
    } else {
        vec![answer(body, result)]
    }
}

/// What the Chat engine's model does, asked with keywords.
fn chat_reply(body: &Value, prompt: &str, result: bool) -> Vec<Value> {
    let messages = body["messages"].as_array().unwrap();
    // Re-sent assistant content continues a paused turn.
    let continued = messages.last().unwrap()["role"] == "assistant";
    // What the member last asked, past the tool results that followed it.
    let asked = messages
        .iter()
        .rev()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .find(|content| !content.contains("tool_result"))
        .unwrap_or_default();
    // A tool call the prompt asks for.
    let asks = |word: &str| !result && body.get("tools").is_some() && prompt.contains(word);
    if continued {
        let citation = |index: &str, cited: &str| {
            json!({"type":"web_search_result_location","url":"https://example.com/a",
                "title":"Example A","encrypted_index":index,"cited_text":cited})
        };
        let citations = [
            citation("opaque-index", "RyzHub released"),
            citation("opaque-index-2", "again"),
        ];
        let answer = "搜索结果显示 RyzHub 已发布";
        vec![json!({"type":"text","text":answer,"citations":citations})]
    } else if asks("websearch") {
        let found = json!({"type":"web_search_result","url":"https://example.com/a",
            "title":"Example A","encrypted_content":"opaque-search","page_age":null});
        let search = json!({"type":"server_tool_use","id":"srvtoolu_1","name":"web_search",
            "input":{"query":"RyzHub"}});
        let results = json!({"type":"web_search_tool_result","tool_use_id":"srvtoolu_1",
            "content":[found]});
        vec![text("先搜索一下"), search, results]
    } else if asks("sharefiles") {
        let cmd = "mkdir -p out && printf 'quarterly report' > out/report.txt && ln -sf /etc/passwd leak && echo made";
        vec![exec(cmd, 10000)]
    } else if asks("sharecheck") {
        vec![exec("cat /attachments/report.txt", 10000)]
    } else if result && asked.contains("sharefiles") {
        vec![text(
            "文件在这里：[报告](sandbox:/workspace/out/report.txt)、[缺失](sandbox:/workspace/missing.txt)、[泄露](sandbox:/workspace/leak)、[目录](sandbox:/workspace/out)，代码里的 `[不算](sandbox:/workspace/out/report.txt)` 不算。",
        )]
    } else if result && asked.contains("sharecheck") {
        vec![text(
            "还是这份：[报告](sandbox:/workspace/out/report.txt)，也就是 [附件里的](/attachments/report.txt)。",
        )]
    } else if asks("attachfiles") {
        let cmd = "cat /attachments/notes.csv; ls /attachments; touch /attachments/new 2>/dev/null || echo READONLY";
        vec![exec(cmd, 10000)]
    } else if asks("fscheck") {
        let cmd = "cat data/notes.csv; printf 'made here' > out.txt";
        vec![
            tool_use(
                "call_a",
                "fs_upload",
                json!({"name":"notes.csv","path":"data/notes.csv"}),
            ),
            tool_use("call_b", "exec", json!({"cmd":cmd,"timeout_ms":10000})),
            tool_use("call_c", "fs_download", json!({"path":"out.txt"})),
        ]
    } else if asks("depscheck") {
        vec![exec("cat /opt/ryzhub-deps/marker", 10000)]
    } else if asks("markercat") {
        vec![exec("printf once >> marker; cat marker", 10000)]
    } else if asks("quotacheck") {
        let cmd = "for f in /workspace/big /root/big; do dd if=/dev/zero of=$f bs=1M count=300 2>&1 | grep -oE 'No space left on device|Disk quota exceeded' | head -1; done; rm -f /workspace/big /root/big; awk 'BEGIN{s=\"x\"; while(1) s=s s}'; echo HOG=$?; echo small > /workspace/ok && echo SMALL_OK";
        vec![exec(cmd, 60000)]
    } else if asks("egresscheck") {
        let cmd = "apk update >/tmp/apk.log 2>&1 && echo APK_OK; wget -q -T 10 -O /dev/null http://example.com/; echo WGET=$?; wget -Y off -q -T 5 -O /dev/null http://1.1.1.1/; echo DIRECT=$?; printenv https_proxy";
        vec![exec(cmd, 60000)]
    } else if asks("streamcheck") {
        vec![exec("for i in 1 2 3; do echo line$i; sleep 1; done", 20000)]
    } else if asks("attachls") {
        vec![exec("ls /attachments", 10000)]
    } else if asks("exec") {
        vec![text("准备执行工具"), exec(command(prompt), 10000)]
    } else if asks("viewimage") {
        let cmd = format!("printf %s {DOT_PNG} | base64 -d > dot.png");
        vec![
            tool_use("call_a", "exec", json!({"cmd":cmd,"timeout_ms":10000})),
            tool_use("call_b", "view_image", json!({"path":"dot.png"})),
        ]
    } else if asks("bigoutput") {
        let id = format!("call_big_{}", Uuid::now_v7().simple());
        let cmd = "head -c 6000 /dev/zero | tr '\\0' a";
        vec![tool_use(&id, "exec", json!({"cmd":cmd,"timeout_ms":10000}))]
    } else if asks("readback") {
        // A result moved out of the context, read back by the id its placeholder names.
        let moved = messages
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|b| b["type"] == "tool_result")
            .find(|b| {
                b["content"]
                    .as_str()
                    .is_some_and(|c| c.starts_with("[Moved out"))
            })
            .map(|b| b["tool_use_id"].clone());
        vec![tool_use(
            "call_read",
            "read_tool_output",
            json!({"id":moved}),
        )]
    } else if asks("plugin") {
        // Plugin tools, by the word after "plugin".
        let (tool, input) = if prompt.contains("pluginlookup") {
            ("acme-tracker__lookup", json!({"id": "7"}))
        } else if prompt.contains("pluginask") {
            ("acme-tracker__ask", json!({}))
        } else if prompt.contains("pluginwidget") {
            ("acme-tracker__widget", json!({}))
        } else {
            (
                "load_skill",
                json!({"plugin": "acme-tracker", "name": "triage"}),
            )
        };
        let id = format!("call_{}", Uuid::now_v7().simple());
        vec![tool_use(&id, tool, input)]
    } else if !result && prompt.contains("autocompact") {
        vec![
            json!({"type":"compaction","content":"自动压缩摘要"}),
            text("压缩后继续"),
        ]
    } else if asks("question") {
        let input = json!({"question":"选择一种颜色"});
        vec![tool_use("call_question", "ask_user", input)]
    } else {
        vec![answer(body, result)]
    }
}

/// OpenAI Responses, as Codex reaches it.
async fn responses(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    requests.lock().unwrap().push((body.clone(), headers));
    let input = body["input"].as_array().unwrap();
    // The member's last message, and whether a tool answered since.
    let asked = input.iter().rposition(|i| i["role"] == "user").unwrap_or(0);
    let prompt = input.get(asked).unwrap_or(&Value::Null).to_string();
    let result = input
        .iter()
        .skip(asked)
        .any(|i| i["type"] == "function_call_output");
    let message = |text: &str, phase: Option<&str>| {
        let content = json!([{"type":"output_text","text":text,"annotations":[]}]);
        let mut item = json!({"id":format!("msg_{}",Uuid::now_v7()),"type":"message",
            "role":"assistant","status":"completed","content":content});
        if let Some(phase) = phase {
            item["phase"] = json!(phase);
        }
        item
    };
    let call = |call_id: &str, name: &str, arguments: Value| {
        json!({"id":format!("fc_{}",Uuid::now_v7()),"type":"function_call","call_id":call_id,
            "name":name,"arguments":arguments.to_string(),"status":"completed"})
    };
    let reasoning = json!({"id":format!("rs_{}",Uuid::now_v7()),"type":"reasoning","summary":[],
        "encrypted_content":"opaque-reasoning"});
    let output = if prompt.contains("HARNESS_FORK_BOUNDARY") {
        // Commentary and a command before the final answer: a turn to fork only after.
        if result {
            vec![message("HARNESS_AFTER_TOOL", Some("final_answer"))]
        } else {
            let arguments = json!({"cmd":"printf HARNESS_TOOL_RESULT","max_output_tokens":100});
            vec![
                message("HARNESS_BEFORE_TOOL", Some("commentary")),
                call("boundary-call", "exec_command", arguments),
            ]
        }
    } else if prompt.contains("native-exec") && !result {
        let marker = "/workspace/approval-marker";
        let command = format!("printf native-once >> {marker}; cat {marker}");
        let (name, arguments) = if offers(&body, "exec_command") {
            (
                "exec_command",
                json!({"cmd":command,"max_output_tokens":100}),
            )
        } else if offers(&body, "exec") {
            let command = format!("printf responses-once >> {marker}; cat {marker}");
            ("exec", json!({"cmd":command}))
        } else {
            (
                "shell_command",
                json!({"command":command,"workdir":"/workspace"}),
            )
        };
        vec![call("fixture-native-command", name, arguments), reasoning]
    } else if prompt.contains("HARNESS_SHARE") {
        vec![message(SHARED_REPLY, None), reasoning]
    } else {
        vec![
            message("Codex 本地协议联调成功：你好 Rust。", None),
            reasoning,
        ]
    };
    let usage = json!({"input_tokens":31,"output_tokens":9,"total_tokens":40,
        "input_tokens_details":{"cached_tokens":7}});
    let id = format!("resp_{}", Uuid::now_v7());
    let response = json!({"id":id,"object":"response","created_at":0,"model":body["model"],
        "status":"completed","output":output,"usage":usage});
    let created = json!({"id":id,"object":"response","status":"in_progress","output":[]});
    let mut events = vec![json!({"type":"response.created","response":created})];
    for (index, item) in output.iter().enumerate() {
        match item["type"].as_str().unwrap() {
            // A message streams its text a word at a time.
            "message" => {
                let mut opened = item.clone();
                opened["status"] = json!("in_progress");
                opened["content"] = json!([]);
                let added = json!({"type":"response.output_item.added","output_index":index,
                    "item":opened});
                events.push(added);
                let text = item["content"][0]["text"].as_str().unwrap();
                for delta in text.split_inclusive(' ') {
                    let delta = json!({"type":"response.output_text.delta","item_id":item["id"],
                        "output_index":index,"content_index":0,"delta":delta});
                    events.push(delta);
                }
            }
            // Reasoning arrives whole.
            "reasoning" => {}
            _ => events.push(
                json!({"type":"response.output_item.added","output_index":index,"item":item}),
            ),
        }
        events.push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
    }
    events.push(json!({"type":"response.completed","response":response}));
    let delay = if prompt.contains("slow") { 1000 } else { 50 };
    stream(events, Duration::from_millis(delay))
}

async fn compact(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    requests.lock().unwrap().push((body, headers));
    let compaction =
        json!({"type":"compaction","id":"cp_1","encrypted_content":"opaque-compaction"});
    let reply = json!({"id":"cmp_local","object":"response.compaction","output":[compaction],
        "usage":{"input_tokens":20,"output_tokens":3}});
    Json(reply)
}

/// One dimension per meaning: texts about 苹果 point one way, all others another.
async fn embeddings(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    requests.lock().unwrap().push((body.clone(), headers));
    let input = body["input"].as_array().unwrap();
    let texts = input.iter().map(|v| v.as_str().unwrap());
    if texts.clone().any(|t| t.contains("embedding-unavailable")) {
        let error = json!({"error":"fixture unavailable"});
        return (StatusCode::SERVICE_UNAVAILABLE, Json(error)).into_response();
    }
    let data: Vec<Value> = texts
        .enumerate()
        .map(|(index, text)| {
            let mut embedding = vec![0.0f32; 1024];
            embedding[if text.contains("苹果") { 0 } else { 1 }] = 1.0;
            json!({"index":index,"embedding":embedding})
        })
        .collect();
    Json(json!({"data":data,"usage":{"prompt_tokens":7,"total_tokens":7}})).into_response()
}

async fn mcp(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    requests.lock().unwrap().push((body.clone(), headers));
    Json(if body["method"] == "initialize" {
        json!({"jsonrpc":"2.0","id":body["id"],"result":{"protocolVersion":"2025-03-26",
            "capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}})
    } else if body["method"] == "tools/list" {
        json!({"jsonrpc":"2.0","id":body["id"],"result":{"tools":[{"name":"write_note",
            "description":"Local test tool","inputSchema":{"type":"object","required":["text"],
            "properties":{"text":{"type":"string"}}}}]}})
    } else {
        json!({"jsonrpc":"2.0","id":body["id"],"result":{
            "content":[{"type":"text","text":"tool executed"}],"isError":false}})
    })
}
