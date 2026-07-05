#[tokio::test]
async fn replay_uses_recorded_response_without_streaming_provider_call() {
    let provider = StreamingFakeProvider::new(text_response("live"), vec!["li", "ve"]);
    let chat_calls = Arc::clone(&provider.chat_calls);
    let stream_calls = Arc::clone(&provider.stream_calls);
    let recorded = text_response("recorded");
    let replay_entries = vec![
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 1,
            entry: JournalEntryKind::TurnStart,
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 2,
            entry: JournalEntryKind::LlmRequest {
                model: "test-model".into(),
                message_count: 2,
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 3,
            entry: JournalEntryKind::LlmResponse {
                model: recorded.model.clone(),
                token_usage: recorded.token_usage.clone(),
                finish_reason: format!("{:?}", recorded.finish_reason),
                assistant_message: Some(recorded.message.clone()),
            },
        },
    ];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(crate::ChannelActivitySink::new(tx));
    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        Box::new(SystemClock),
        Some(replay_entries),
    );
    agent.sink = sink;

    let output = agent.run("replay").await.unwrap();
    assert_eq!(output.exit_reason, ExitReason::Complete);
    assert_eq!(chat_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(stream_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(output
        .messages
        .iter()
        .any(|message| message.role == Role::Assistant && message.content == "recorded"));

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert_eq!(collect_token_texts(&events), vec!["recorded"]);
}

#[tokio::test]
async fn replay_resume_preserves_provider_native_content_for_live_continuation() {
    let provider_content = vec![simulacra_types::ProviderContentBlock {
        provider: "anthropic".into(),
        value: serde_json::json!({
            "type": "thinking",
            "thinking": "replayed thought",
            "signature": "sig-replay"
        }),
    }];
    let assistant_message = Message {
        role: Role::Assistant,
        content: String::new(),
        tool_calls: vec![ToolCallMessage {
            id: "toolu_replay".into(),
            name: "echo".into(),
            arguments: serde_json::json!({"msg": "from replay"}),
        }],
        tool_call_id: None,
        provider_content: provider_content.clone(),
    };
    let replay_entries = vec![
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 1,
            entry: JournalEntryKind::TurnStart,
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 2,
            entry: JournalEntryKind::LlmRequest {
                model: "test-model".into(),
                message_count: 2,
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 3,
            entry: JournalEntryKind::LlmResponse {
                model: "claude-fable-5".into(),
                token_usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
                finish_reason: "ToolUse".into(),
                assistant_message: Some(assistant_message),
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 4,
            entry: JournalEntryKind::ToolCall {
                tool_call_id: Some("toolu_replay".into()),
                tool_name: "echo".into(),
                arguments: serde_json::json!({"msg": "from replay"}),
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 5,
            entry: JournalEntryKind::ToolResult {
                tool_call_id: Some("toolu_replay".into()),
                tool_name: "echo".into(),
                content: "{\"msg\":\"from replay\"}".into(),
                is_error: false,
            },
        },
    ];
    let captured_calls = Arc::new(Mutex::new(Vec::new()));
    let provider = CapturingSequencedProvider {
        responses: Mutex::new(vec![text_response("live after replay")]),
        calls: Arc::clone(&captured_calls),
    };
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(EchoTool))
        .expect("test tool registration should succeed");

    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(provider),
        tools,
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        Box::new(SystemClock),
        Some(replay_entries),
    );

    let output = agent.run("replay then continue").await.unwrap();

    assert_eq!(output.exit_reason, ExitReason::Complete);
    let calls = captured_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let assistant_turn = calls[0]
        .iter()
        .find(|message| {
            message.role == Role::Assistant
                && message
                    .tool_calls
                    .iter()
                    .any(|tool_call| tool_call.id == "toolu_replay")
        })
        .expect("live continuation should include replayed assistant tool-use turn");
    assert_eq!(assistant_turn.provider_content, provider_content);
}

#[tokio::test]
async fn replay_does_not_emit_tool_call_deltas() {
    let provider = StreamingFakeProvider::with_events(
        text_response("live"),
        vec![simulacra_types::ProviderStreamEvent::ToolCallDelta {
            index: 0,
            tool_call_id: Some("live-call".into()),

            name: Some("file_read".into()),
            arguments_delta: "{\"path\":\"/live\"}".into(),
        }],
    );
    let recorded = text_response("recorded");
    let replay_entries = vec![
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 1,
            entry: JournalEntryKind::TurnStart,
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 2,
            entry: JournalEntryKind::LlmRequest {
                model: "test-model".into(),
                message_count: 2,
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 3,
            entry: JournalEntryKind::LlmResponse {
                model: recorded.model.clone(),
                token_usage: recorded.token_usage.clone(),
                finish_reason: format!("{:?}", recorded.finish_reason),
                assistant_message: Some(recorded.message.clone()),
            },
        },
    ];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(crate::ChannelActivitySink::new(tx));
    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        Box::new(SystemClock),
        Some(replay_entries),
    );
    agent.sink = sink;

    let output = agent.run("replay").await.unwrap();
    assert_eq!(output.exit_reason, ExitReason::Complete);

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(collect_tool_call_deltas(&events).is_empty());
}
