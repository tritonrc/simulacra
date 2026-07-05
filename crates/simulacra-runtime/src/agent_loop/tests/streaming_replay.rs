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
