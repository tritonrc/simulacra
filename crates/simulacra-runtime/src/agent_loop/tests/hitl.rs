use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

fn hitl_journal_entry(timestamp_ms: u64, entry: JournalEntryKind) -> JournalEntry {
    JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: AgentId("test-agent".into()),
        timestamp_ms,
        entry,
    }
}

fn hitl_response(
    id: &str,
    tool_name: &str,
    args: serde_json::Value,
) -> simulacra_types::ProviderResponse {
    simulacra_types::ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCallMessage {
                id: id.into(),
                name: tool_name.into(),
                arguments: args,
            }],
            tool_call_id: None,
            provider_content: vec![],
        },
        token_usage: TokenUsage {
            input_tokens: 20,
            output_tokens: 10,
        },
        finish_reason: FinishReason::ToolUse,
        provider_response_id: Some("resp-hitl".into()),
        model: "test-model".into(),
    }
}

fn hitl_loop(
    provider: FakeProvider,
    tools: ToolRegistry,
    hitl: AgentHitlRuntime,
    sink: Arc<dyn ActivitySink>,
    journal: Arc<dyn JournalStorage>,
) -> AgentLoop {
    let mut agent = AgentLoop::new(
        default_config(),
        Box::new(provider),
        tools,
        Box::new(PassthroughContext),
        journal,
        default_budget(),
        Some(sink),
        None,
    );
    agent.set_hitl_runtime(hitl);
    agent
}

async fn recv_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ActivityEvent>,
    predicate: impl Fn(&ActivityEvent) -> bool,
) -> ActivityEvent {
    loop {
        let event = rx.recv().await.expect("activity event should arrive");
        if predicate(&event) {
            return event;
        }
    }
}

#[tokio::test]
async fn request_input_tool_waits_for_input_response_and_journals_tool_result() {
    let (senders, hitl) = AgentHitlRuntime::channel_pair(false);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink: Arc<dyn ActivitySink> = Arc::new(crate::ChannelActivitySink::new(event_tx));
    let journal = Arc::new(InMemoryJournalStorage::new());
    let journal_for_loop: Arc<dyn JournalStorage> = journal.clone();

    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(RequestInputTool::new(
            hitl.clone(),
            Arc::clone(&sink),
        )))
        .expect("request_input registration should succeed");

    let mut agent = hitl_loop(
        FakeProvider::new(vec![
            hitl_response(
                "input-1",
                REQUEST_INPUT_TOOL_NAME,
                serde_json::json!({"prompt": "Need more detail"}),
            ),
            text_response("done"),
        ]),
        tools,
        hitl,
        sink,
        journal_for_loop,
    );

    let handle = tokio::spawn(async move { agent.run("ask user").await.unwrap() });

    let event = recv_until(&mut event_rx, |event| {
        matches!(event, ActivityEvent::InputRequired { .. })
    })
    .await;
    match event {
        ActivityEvent::InputRequired { prompt, schema } => {
            assert_eq!(prompt, "Need more detail");
            assert!(schema.is_none());
        }
        other => panic!("expected InputRequired, got {other:?}"),
    }

    senders
        .input_tx
        .send("human supplied context".into())
        .await
        .expect("input response should send");

    let output = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("agent should resume after input")
        .unwrap();
    assert_eq!(output.exit_reason, ExitReason::Complete);
    assert!(output
        .messages
        .iter()
        .any(|message| message.role == Role::Tool
            && message.content == "human supplied context"));

    let entries = journal.read_all(&AgentId("test-agent".into())).unwrap();
    assert!(entries.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::ToolResult {
                tool_call_id: Some(id),
                tool_name,
                content,
                is_error: false,
            } if id == "input-1"
                && tool_name == REQUEST_INPUT_TOOL_NAME
                && content == "human supplied context"
        )
    }));
}

#[tokio::test]
async fn tool_approval_required_emits_before_tool_start_and_approval_executes() {
    let (senders, hitl) = AgentHitlRuntime::channel_pair(true);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink: Arc<dyn ActivitySink> = Arc::new(crate::ChannelActivitySink::new(event_tx));
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool)).unwrap();
    let mut agent = hitl_loop(
        FakeProvider::new(vec![hitl_response(
            "approve-1",
            "echo",
            serde_json::json!({"msg": "run"}),
        )]),
        tools,
        hitl,
        Arc::clone(&sink),
        Arc::new(InMemoryJournalStorage::new()),
    );

    let handle = tokio::spawn(async move {
        let mut messages = conversation("approve tool");
        let result = agent.run_single_turn(&mut messages).await.unwrap();
        (result, messages)
    });

    let first = event_rx.recv().await.expect("approval event should arrive");
    assert!(
        matches!(first, ActivityEvent::ToolApprovalRequired { ref tool_call_id, .. } if tool_call_id == "approve-1"),
        "approval must be emitted before ToolStart; got {first:?}"
    );
    senders
        .approval_tx
        .send(ToolApprovalResponse {
            tool_call_id: "approve-1".into(),
            approved: true,
            reason: None,
        })
        .await
        .expect("approval response should send");

    let (result, messages) = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("approved tool should finish")
        .unwrap();
    assert!(matches!(result, TurnResult::ToolCallsProcessed { .. }));
    assert!(messages
        .iter()
        .any(|message| message.role == Role::Tool && message.content.contains("\"msg\":\"run\"")));

    let mut saw_tool_start = false;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(event, ActivityEvent::ToolStart { tool_call_id, .. } if tool_call_id == "approve-1")
        {
            saw_tool_start = true;
        }
    }
    assert!(saw_tool_start, "approved tool should emit ToolStart");
}

#[tokio::test]
async fn mcp_meta_tool_approval_events_redact_arguments_while_ordinary_tools_remain_unchanged() {
    let (senders, hitl) = AgentHitlRuntime::channel_pair(true);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink: Arc<dyn ActivitySink> = Arc::new(crate::ChannelActivitySink::new(event_tx));
    let search_args = serde_json::json!({"query":"QUERYSECRET Authorization: Bearer SEARCHAUTH"});
    let remote_args = serde_json::json!({"token":"CALLSECRET","module_path":"/private/CALLMODULE.wasm"});
    let call_args = serde_json::json!({"server":"github","tool":"issues","arguments":remote_args});
    let response = ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![
                ToolCallMessage { id: "search-approval".into(), name: "mcp_search".into(), arguments: search_args.clone() },
                ToolCallMessage { id: "call-approval".into(), name: "mcp_call".into(), arguments: call_args.clone() },
                ToolCallMessage { id: "echo-approval".into(), name: "echo".into(), arguments: serde_json::json!({"ordinary":"kept"}) },
            ],
            tool_call_id: None,
            provider_content: Vec::new(),
        },
        token_usage: TokenUsage::default(),
        finish_reason: FinishReason::ToolUse,
        provider_response_id: Some("approval-response".into()),
        model: "test-model".into(),
    };
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool)).unwrap();
    tools.register(Box::new(CapturingMetaTool { name: "mcp_search", calls: Arc::new(Mutex::new(Vec::new())) })).unwrap();
    tools.register(Box::new(CapturingMetaTool { name: "mcp_call", calls: Arc::new(Mutex::new(Vec::new())) })).unwrap();
    let mut agent = hitl_loop(
        FakeProvider::new(vec![response]),
        tools,
        hitl,
        Arc::clone(&sink),
        Arc::new(InMemoryJournalStorage::new()),
    );
    let handle = tokio::spawn(async move {
        let mut messages = conversation("approve MCP tools");
        agent.run_single_turn(&mut messages).await.unwrap()
    });

    let mut approvals = Vec::new();
    while approvals.len() < 3 {
        let event = event_rx.recv().await.expect("activity event");
        if let ActivityEvent::ToolApprovalRequired { tool_call_id, name, arguments, .. } = event {
            approvals.push((name, arguments));
            senders.approval_tx.send(ToolApprovalResponse { tool_call_id, approved: true, reason: None }).await.unwrap();
        }
    }
    handle.await.unwrap();
    assert_eq!(
        approvals,
        vec![
            ("mcp_search".into(), serde_json::json!({"query_length":search_args["query"].as_str().unwrap().len()})),
            ("mcp_call".into(), serde_json::json!({"server":"github","tool":"issues","argument_length":remote_args.to_string().len()})),
            ("echo".into(), serde_json::json!({"ordinary":"kept"})),
        ]
    );
    let rendered = format!("{approvals:?}");
    for secret in ["QUERYSECRET", "SEARCHAUTH", "CALLSECRET", "CALLMODULE"] {
        assert!(!rendered.contains(secret));
    }
}

struct CountingHitlTool {
    calls: Arc<AtomicUsize>,
}

impl simulacra_types::Tool for CountingHitlTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "counting_hitl".into(),
            description: "Counts executions".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn call(
        &self,
        _arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(serde_json::json!({"executed": true}))
        })
    }
}

#[tokio::test]
async fn tool_approval_denial_returns_error_result_without_executing_tool() {
    let (senders, hitl) = AgentHitlRuntime::channel_pair(true);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sink: Arc<dyn ActivitySink> = Arc::new(crate::ChannelActivitySink::new(event_tx));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(CountingHitlTool {
            calls: Arc::clone(&calls),
        }))
        .unwrap();
    let mut agent = hitl_loop(
        FakeProvider::new(vec![hitl_response(
            "deny-1",
            "counting_hitl",
            serde_json::json!({}),
        )]),
        tools,
        hitl,
        sink,
        Arc::new(InMemoryJournalStorage::new()),
    );

    let handle = tokio::spawn(async move {
        let mut messages = conversation("deny tool");
        let result = agent.run_single_turn(&mut messages).await.unwrap();
        (result, messages)
    });

    recv_until(&mut event_rx, |event| {
        matches!(event, ActivityEvent::ToolApprovalRequired { tool_call_id, .. } if tool_call_id == "deny-1")
    })
    .await;
    senders
        .approval_tx
        .send(ToolApprovalResponse {
            tool_call_id: "deny-1".into(),
            approved: false,
            reason: Some("not allowed".into()),
        })
        .await
        .expect("denial response should send");

    let (result, messages) = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("denied tool should finish")
        .unwrap();
    assert!(matches!(result, TurnResult::ToolCallsProcessed { .. }));
    assert_eq!(
        calls.load(AtomicOrdering::SeqCst),
        0,
        "denied tool must not execute"
    );
    assert!(messages.iter().any(|message| {
        message.role == Role::Tool && message.content == "ERROR: approval denied: not allowed"
    }));
    while let Ok(event) = event_rx.try_recv() {
        assert!(
            !matches!(event, ActivityEvent::ToolStart { tool_call_id, .. } if tool_call_id == "deny-1"),
            "denied tool must not emit ToolStart"
        );
    }
}

#[tokio::test]
async fn replay_consumes_recorded_hitl_tool_result_without_waiting() {
    let (_senders, hitl) = AgentHitlRuntime::channel_pair(true);
    let assistant_message = hitl_response(
        "replay-input",
        REQUEST_INPUT_TOOL_NAME,
        serde_json::json!({"prompt": "Need input"}),
    )
    .message;
    let replay_entries = vec![
        hitl_journal_entry(1, JournalEntryKind::TurnStart),
        hitl_journal_entry(
            2,
            JournalEntryKind::LlmRequest {
                model: "test-model".into(),
                message_count: 2,
            },
        ),
        hitl_journal_entry(
            3,
            JournalEntryKind::LlmResponse {
                model: "test-model".into(),
                token_usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
                finish_reason: "ToolUse".into(),
                assistant_message: Some(assistant_message),
            },
        ),
        hitl_journal_entry(
            4,
            JournalEntryKind::ToolCall {
                tool_call_id: Some("replay-input".into()),

                tool_name: REQUEST_INPUT_TOOL_NAME.into(),
                arguments: serde_json::json!({"prompt": "Need input"}),
            },
        ),
        hitl_journal_entry(
            5,
            JournalEntryKind::ToolResult {
                tool_call_id: Some("replay-input".into()),

                tool_name: REQUEST_INPUT_TOOL_NAME.into(),
                content: "recorded human response".into(),
                is_error: false,
            },
        ),
    ];
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(EchoTool))
        .expect("dummy registration should succeed");
    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(FakeProvider::new(vec![])),
        tools,
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        Box::new(FixedClock(1000)),
        Some(replay_entries),
    );
    agent.set_hitl_runtime(hitl);
    let mut messages = conversation("replay hitl");

    let (result, messages) = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        let result = agent.run_single_turn(&mut messages).await.unwrap();
        (result, messages)
    })
    .await
    .expect("replay must not wait for HITL input");

    assert!(matches!(result, TurnResult::ToolCallsProcessed { .. }));
    assert!(messages.iter().any(|message| {
        message.role == Role::Tool && message.content == "recorded human response"
    }));
}
