use super::*;

fn profile() -> AcpProfile {
    AcpProfile {
        name: "opencode".into(),
        model_option: "anthropic/{model}".into(),
        protocol: "anthropic".into(),
        ..Default::default()
    }
}

fn settings(approval: &str, plan: bool, model: &str) -> TurnSettings {
    TurnSettings {
        model: model.into(),
        reasoning: "medium".into(),
        approval: approval.into(),
        plan,
    }
}

/// What went to the agent, and what to the guest.
fn split(outs: Vec<Out>) -> (Vec<Value>, Vec<Incoming>) {
    let (mut agent, mut guest) = (vec![], vec![]);
    for out in outs {
        match out {
            Out::Agent(message) => agent.push(message),
            Out::Guest(incoming) => guest.push(*incoming),
        }
    }
    (agent, guest)
}

fn update(session: &str, update: Value) -> Value {
    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}})
}

fn answer_to(message: &Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":message["id"],"result":result})
}

fn events(guest: &[Incoming]) -> Vec<&HarnessEvent> {
    guest
        .iter()
        .filter_map(|i| match i {
            Incoming::Event(event) => Some(event),
            _ => None,
        })
        .collect()
}

fn response(guest: &[Incoming], op: &str) -> Value {
    guest
        .iter()
        .find_map(|i| match i {
            Incoming::Response { id, response } if id == op => Some(response.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no response to {op} in {guest:?}"))
}

const SESSION: &str = "ses_1";

/// A bridge with an initialized agent and an open session in mode `build`, model `m1`.
fn opened(approval: &str) -> Bridge {
    let mut bridge = Bridge::new(profile(), vec!["m1".into(), "m2".into()], true);
    let init = bridge.initialize();
    let start = HarnessRequest::StartThread {
        settings: settings(approval, false, "m1"),
        instructions: String::new(),
    };
    assert!(bridge.request("op-thread", &start, vec![]).is_empty());
    let (agent, _) = split(bridge.incoming(&answer_to(
        &init,
        json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true,"mcpCapabilities":{"http":true},"promptCapabilities":{"image":true}}}),
    )));
    assert_eq!(agent[0]["method"], "session/new");
    assert_eq!(agent[0]["params"]["mcpServers"][0]["url"], MCP_URL);
    let options = json!([
        {"id":"model","type":"select","currentValue":"anthropic/m1","options":[{"value":"anthropic/m1"},{"value":"anthropic/m2"}]},
        {"id":"mode","type":"select","currentValue":"build","options":[{"value":"build"},{"value":"plan"}]}
    ]);
    let (more, guest) = split(bridge.incoming(&answer_to(
        &agent[0],
        json!({"sessionId":SESSION,"configOptions":options}),
    )));
    assert!(more.is_empty(), "nothing to change: {more:?}");
    assert_eq!(
        response(&guest, "op-thread")["result"]["thread"]["id"],
        SESSION
    );
    bridge
}

fn start_turn(
    bridge: &mut Bridge,
    op: &str,
    approval: &str,
    plan: bool,
    model: &str,
) -> (String, Vec<Value>) {
    let request = HarnessRequest::StartTurn {
        thread_id: SESSION.into(),
        text: "hello".into(),
        settings: settings(approval, plan, model),
        images: vec![],
    };
    let (agent, guest) = split(bridge.request(op, &request, vec![]));
    let turn = response(&guest, op)["result"]["turn"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        matches!(events(&guest)[..], [HarnessEvent::TurnStarted { turn_id }] if *turn_id == turn)
    );
    (turn, agent)
}

#[test]
fn a_turn_streams_its_reply_runs_an_approved_command_and_completes() {
    let mut bridge = opened("ask");
    let (turn, agent) = start_turn(&mut bridge, "op-turn", "ask", false, "m1");
    let prompt = &agent[0];
    assert_eq!(prompt["method"], "session/prompt");
    assert_eq!(prompt["params"]["prompt"][0]["text"], "hello");

    let chunk = |text: &str| {
        update(
            SESSION,
            json!({"sessionUpdate":"agent_message_chunk","messageId":"msg_a","content":{"type":"text","text":text}}),
        )
    };
    let (_, guest) = split(bridge.incoming(&chunk("你好，")));
    let (_, more) = split(bridge.incoming(&chunk("世界")));
    let reply = format!("{turn}:msg_a");
    assert!(
        matches!(events(&guest)[..], [HarnessEvent::Item { phase: ItemPhase::Started, kind: ItemKind::Assistant, native_id, .. }] if *native_id == reply)
    );
    assert!(
        matches!(&more[..], [Incoming::Delta { item_id, stream: "text", text }] if *item_id == reply && text == "世界")
    );

    // The tool call ends the reply.
    let (_, guest) = split(bridge.incoming(&update(SESSION, json!({"sessionUpdate":"tool_call","toolCallId":"toolu_1","title":"bash","kind":"execute","status":"pending","rawInput":{}}))));
    let events_now = events(&guest);
    assert!(
        matches!(events_now[0], HarnessEvent::Item { phase: ItemPhase::Completed, content, .. } if content.text == "你好，世界")
    );
    assert!(matches!(
        events_now[1],
        HarnessEvent::Item {
            phase: ItemPhase::Started,
            kind: ItemKind::Command,
            ..
        }
    ));

    let permission = json!({"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"sessionId":SESSION,
        "toolCall":{"toolCallId":"toolu_1","title":"echo hi","kind":"execute","status":"pending","rawInput":{"command":"echo hi"}},
        "options":[{"optionId":"once","kind":"allow_once","name":"Allow once"},{"optionId":"always","kind":"allow_always","name":"Always"},{"optionId":"reject","kind":"reject_once","name":"Reject"}]}});
    let (agent, guest) = split(bridge.incoming(&permission));
    assert!(agent.is_empty(), "asks first: {agent:?}");
    let HarnessEvent::ApprovalRequested {
        request_id,
        category,
        summary,
        ..
    } = events(&guest)[0]
    else {
        panic!("{guest:?}");
    };
    assert_eq!(
        (request_id.as_str(), *category, summary.as_str()),
        ("0", ApprovalCategory::Command, "echo hi")
    );
    let (agent, _) = split(bridge.answer("0", &HarnessAnswer::Approval { allow: true }));
    assert_eq!(
        agent[0],
        json!({"jsonrpc":"2.0","id":0,"result":{"outcome":{"outcome":"selected","optionId":"once"}}})
    );
    assert!(
        bridge
            .answer("0", &HarnessAnswer::Approval { allow: true })
            .is_empty(),
        "answered once"
    );

    let (_, guest) = split(bridge.incoming(&update(SESSION, json!({"sessionUpdate":"tool_call_update","toolCallId":"toolu_1","status":"in_progress","content":[{"type":"content","content":{"type":"text","text":"hi"}}]}))));
    assert!(
        matches!(&guest[..], [Incoming::Delta { stream: "terminal_output", text, .. }] if text == "hi")
    );
    let (_, guest) = split(bridge.incoming(&update(SESSION, json!({"sessionUpdate":"tool_call_update","toolCallId":"toolu_1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"hi\n"}}]}))));
    let HarnessEvent::Item {
        phase: ItemPhase::Completed,
        kind: ItemKind::Command,
        content,
        ..
    } = events(&guest)[0]
    else {
        panic!("{guest:?}");
    };
    assert_eq!(
        (content.name.as_deref(), content.output.as_deref()),
        (Some("echo hi"), Some("hi\n"))
    );

    // The same message continuing after the tool is another item.
    let (_, guest) = split(bridge.incoming(&chunk("完成")));
    assert!(
        matches!(events(&guest)[..], [HarnessEvent::Item { native_id, .. }] if *native_id == format!("{reply}:1"))
    );

    let (_, guest) = split(bridge.incoming(&answer_to(prompt, json!({"stopReason":"end_turn"}))));
    let events_now = events(&guest);
    assert!(
        matches!(events_now[0], HarnessEvent::Item { phase: ItemPhase::Completed, content, .. } if content.text == "完成")
    );
    assert!(
        matches!(events_now[1], HarnessEvent::TurnCompleted { status: TurnStatus::Completed, turn_id } if turn_id.as_deref() == Some(turn.as_str()))
    );
}

#[test]
fn settings_switch_mode_and_model_before_the_prompt() {
    let mut bridge = opened("ask");
    let (_, agent) = start_turn(&mut bridge, "op-turn", "ask", true, "m2");
    // Options first; the prompt waits for them.
    let methods: Vec<&str> = agent.iter().filter_map(|m| m["method"].as_str()).collect();
    assert_eq!(
        methods,
        ["session/set_config_option", "session/set_config_option"]
    );
    assert_eq!(
        agent[0]["params"],
        json!({"sessionId":SESSION,"configId":"mode","value":"plan"})
    );
    assert_eq!(agent[1]["params"]["value"], "anthropic/m2");
    let (none, _) = split(bridge.incoming(&answer_to(&agent[0], json!({"configOptions":[]}))));
    assert!(none.is_empty());
    let (prompt, _) = split(bridge.incoming(&answer_to(&agent[1], json!({"configOptions":[]}))));
    assert_eq!(prompt[0]["method"], "session/prompt");
    bridge.incoming(&answer_to(&prompt[0], json!({"stopReason":"end_turn"})));
    // Leaving the plan returns to the mode the session worked in.
    let (_, agent) = start_turn(&mut bridge, "op-next", "ask", false, "m2");
    assert_eq!(agent[0]["params"]["value"], "build");
}

#[test]
fn approval_modes_answer_what_they_cover() {
    let mut bridge = opened("allow_edits");
    let (_, _) = start_turn(&mut bridge, "op-turn", "allow_edits", false, "m1");
    let ask = |id: u64, kind: &str| {
        json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{"sessionId":SESSION,
        "toolCall":{"toolCallId":format!("t{id}"),"title":"x","kind":kind},
        "options":[{"optionId":"yes","kind":"allow_once","name":"Yes"},{"optionId":"no","kind":"reject_once","name":"No"}]}})
    };
    let (agent, guest) = split(bridge.incoming(&ask(1, "edit")));
    assert_eq!(agent[0]["result"]["outcome"]["optionId"], "yes");
    assert!(
        !events(&guest)
            .iter()
            .any(|e| matches!(e, HarnessEvent::ApprovalRequested { .. }))
    );
    let (agent, guest) = split(bridge.incoming(&ask(2, "execute")));
    assert!(agent.is_empty());
    assert!(
        events(&guest)
            .iter()
            .any(|e| matches!(e, HarnessEvent::ApprovalRequested { .. }))
    );
    let (agent, _) = split(bridge.answer("2", &HarnessAnswer::Approval { allow: false }));
    assert_eq!(agent[0]["result"]["outcome"]["optionId"], "no");

    let mut bridge = opened("full_auto");
    start_turn(&mut bridge, "op-turn", "full_auto", false, "m1");
    let (agent, _) = split(bridge.incoming(&ask(3, "execute")));
    assert_eq!(agent[0]["result"]["outcome"]["optionId"], "yes");
}

#[test]
fn an_interrupt_cancels_the_prompt_and_open_permissions() {
    let mut bridge = opened("ask");
    let (turn, agent) = start_turn(&mut bridge, "op-turn", "ask", false, "m1");
    let permission = json!({"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{"sessionId":SESSION,
        "toolCall":{"toolCallId":"t1","title":"rm","kind":"execute"},"options":[{"optionId":"once","kind":"allow_once","name":"Allow"}]}});
    bridge.incoming(&permission);
    let interrupt = HarnessRequest::InterruptTurn {
        thread_id: SESSION.into(),
        turn_id: turn.clone(),
    };
    let (sent, guest) = split(bridge.request("op-stop", &interrupt, vec![]));
    assert!(sent.contains(
        &json!({"jsonrpc":"2.0","id":"p1","result":{"outcome":{"outcome":"cancelled"}}})
    ));
    assert!(sent.iter().any(|m| m["method"] == "session/cancel"));
    assert!(response(&guest, "op-stop").get("error").is_none());
    let (_, guest) =
        split(bridge.incoming(&answer_to(&agent[0], json!({"stopReason":"cancelled"}))));
    let events_now = events(&guest);
    // The tool never finished; it is closed with the turn.
    assert!(matches!(
        events_now[0],
        HarnessEvent::Item {
            phase: ItemPhase::Completed,
            kind: ItemKind::Command,
            ..
        }
    ));
    assert!(matches!(
        events_now[1],
        HarnessEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            ..
        }
    ));
}

#[test]
fn a_loaded_session_replays_history_quietly() {
    let mut bridge = Bridge::new(profile(), vec!["m1".into()], false);
    let init = bridge.initialize();
    bridge.incoming(&answer_to(
        &init,
        json!({"agentCapabilities":{"loadSession":true}}),
    ));
    let resume = HarnessRequest::ResumeThread {
        thread_id: SESSION.into(),
        settings: settings("ask", false, "m1"),
        instructions: String::new(),
    };
    let (agent, _) = split(bridge.request("op-resume", &resume, vec![]));
    assert_eq!(agent[0]["method"], "session/load");
    assert_eq!(agent[0]["params"]["mcpServers"], json!([]));
    let (_, guest) = split(bridge.incoming(&update(SESSION, json!({"sessionUpdate":"agent_message_chunk","messageId":"old","content":{"type":"text","text":"history"}}))));
    assert!(guest.is_empty(), "{guest:?}");
    let (_, guest) = split(bridge.incoming(&update(SESSION, json!({"sessionUpdate":"available_commands_update","availableCommands":[{"name":"review","description":"Review changes"}]}))));
    assert!(
        matches!(events(&guest)[..], [HarnessEvent::Capabilities { commands, .. }] if commands[0]["name"] == "review")
    );
    let (_, guest) = split(bridge.incoming(&answer_to(&agent[0], json!({}))));
    assert_eq!(
        response(&guest, "op-resume")["result"]["thread"]["id"],
        SESSION
    );
    // Resuming the session already open needs no call.
    let (agent, guest) = split(bridge.request("op-again", &resume, vec![]));
    assert!(agent.is_empty());
    assert_eq!(
        response(&guest, "op-again")["result"]["thread"]["id"],
        SESSION
    );
    // What ACP lacks is refused.
    let fork = HarnessRequest::ForkThread {
        thread_id: SESSION.into(),
        last_turn_id: None,
        message_id: None,
        settings: settings("ask", false, "m1"),
        instructions: String::new(),
    };
    let (_, guest) = split(bridge.request("op-fork", &fork, vec![]));
    assert!(response(&guest, "op-fork")["error"]["message"].is_string());
}

#[test]
fn thoughts_diffs_and_plans_become_items() {
    let mut bridge = opened("full_auto");
    let (turn, agent) = start_turn(&mut bridge, "op-turn", "full_auto", false, "m1");
    let thought = |text: &str| {
        update(
            SESSION,
            json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":text}}),
        )
    };
    let (_, guest) = split(bridge.incoming(&thought("think ")));
    assert!(matches!(
        events(&guest)[..],
        [HarnessEvent::Item {
            phase: ItemPhase::Started,
            kind: ItemKind::Reasoning,
            ..
        }]
    ));
    let (_, guest) = split(bridge.incoming(&thought("more")));
    assert!(guest.is_empty(), "reasoning arrives whole");
    let (_, guest) = split(bridge.incoming(&update(SESSION, json!({"sessionUpdate":"plan","entries":[
        {"content":"read","priority":"high","status":"completed"},{"content":"write","priority":"high","status":"pending"}]}))));
    // A plan is not text being written: the thought goes on.
    let HarnessEvent::Item {
        kind: ItemKind::Plan,
        content,
        native_id,
        ..
    } = events(&guest)[0]
    else {
        panic!("{guest:?}");
    };
    assert_eq!(
        (content.text.as_str(), native_id.as_str()),
        ("- [x] read\n- [ ] write", format!("{turn}:plan").as_str())
    );
    let (_, guest) = split(bridge.incoming(&update(SESSION, json!({"sessionUpdate":"tool_call","toolCallId":"e1","title":"edit","kind":"edit","status":"completed",
        "content":[{"type":"diff","path":"/workspace/a.txt","oldText":"one\ntwo\n","newText":"one\nthree\n"},{"type":"diff","path":"/workspace/b.txt","newText":"new\n"}]}))));
    let events_now = events(&guest);
    assert!(
        matches!(events_now[0], HarnessEvent::Item { phase: ItemPhase::Completed, kind: ItemKind::Reasoning, content, .. } if content.text == "think more")
    );
    let HarnessEvent::Item {
        phase: ItemPhase::Completed,
        kind: ItemKind::FileChange,
        content,
        ..
    } = events_now[2]
    else {
        panic!("{guest:?}");
    };
    assert_eq!(content.file_changes[0].change, "update");
    assert!(
        content.file_changes[0].diff.contains("-two\n+three\n"),
        "{}",
        content.file_changes[0].diff
    );
    assert_eq!(content.file_changes[1].change, "add");
    let (_, guest) = split(bridge.incoming(&answer_to(&agent[0], json!({"stopReason":"refusal"}))));
    assert!(matches!(
        events(&guest)[..],
        [HarnessEvent::TurnCompleted {
            status: TurnStatus::Failed,
            ..
        }]
    ));
}

#[test]
fn a_failed_initialize_fails_every_request() {
    let mut bridge = Bridge::new(profile(), vec![], false);
    let init = bridge.initialize();
    bridge.request("op-early", &HarnessRequest::ListModels, vec![]);
    let failed = json!({"jsonrpc":"2.0","id":init["id"],"error":{"code":-32603,"message":"boom"}});
    let (_, guest) = split(bridge.incoming(&failed));
    assert_eq!(response(&guest, "op-early")["error"]["message"], "boom");
    let (_, guest) = split(bridge.request("op-late", &HarnessRequest::ListModels, vec![]));
    assert_eq!(response(&guest, "op-late")["error"]["message"], "boom");
    // Requests from the agent the guest does not serve are refused.
    let mut bridge = opened("ask");
    let (agent, _) = split(
        bridge.incoming(&json!({"jsonrpc":"2.0","id":9,"method":"fs/read_text_file","params":{}})),
    );
    assert_eq!(agent[0]["error"]["code"], -32601);
}
