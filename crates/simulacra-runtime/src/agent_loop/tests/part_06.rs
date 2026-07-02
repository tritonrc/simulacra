struct StreamingFakeProvider {
    response: ProviderResponse,
    events: Vec<simulacra_types::ProviderStreamEvent>,
    block_after_chunks: bool,
    chat_calls: Arc<std::sync::atomic::AtomicUsize>,
    stream_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl StreamingFakeProvider {
    fn new(response: ProviderResponse, chunks: Vec<&str>) -> Self {
        Self {
            response,
            events: chunks
                .into_iter()
                .map(|text| simulacra_types::ProviderStreamEvent::TextDelta { text: text.into() })
                .collect(),
            block_after_chunks: false,
            chat_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            stream_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn with_events(
        response: ProviderResponse,
        events: Vec<simulacra_types::ProviderStreamEvent>,
    ) -> Self {
        Self {
            response,
            events,
            block_after_chunks: false,
            chat_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            stream_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn blocking_with_events(
        response: ProviderResponse,
        events: Vec<simulacra_types::ProviderStreamEvent>,
    ) -> Self {
        Self {
            block_after_chunks: true,
            ..Self::with_events(response, events)
        }
    }

    fn blocking(response: ProviderResponse, chunks: Vec<&str>) -> Self {
        Self {
            block_after_chunks: true,
            ..Self::new(response, chunks)
        }
    }
}

impl Provider for StreamingFakeProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        self.chat_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let response = self.response.clone();
        Box::pin(async move { Ok(response) })
    }

    fn as_streaming(&self) -> Option<&dyn simulacra_types::StreamingProvider> {
        Some(self)
    }
}

impl simulacra_types::StreamingProvider for StreamingFakeProvider {
    fn chat_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
        sink: &'a dyn simulacra_types::ProviderStreamSink,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        self.stream_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let response = self.response.clone();
        let events = self.events.clone();
        let block_after_chunks = self.block_after_chunks;
        Box::pin(async move {
            for event in events {
                sink.emit(event);
                tokio::task::yield_now().await;
            }
            if block_after_chunks {
                std::future::pending::<()>().await;
            }
            Ok(response)
        })
    }
}

fn collect_token_texts(events: &[ActivityEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            ActivityEvent::Token { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn collect_tool_call_deltas(events: &[ActivityEvent]) -> Vec<ActivityEvent> {
    events
        .iter()
        .filter(|event| matches!(event, ActivityEvent::ToolCallDelta { .. }))
        .cloned()
        .collect()
}

#[tokio::test]
async fn streaming_provider_tokens_emit_in_order_and_final_response_is_journaled_once() {
    let provider = StreamingFakeProvider::new(text_response("Hello"), vec!["Hel", "lo"]);
    let chat_calls = Arc::clone(&provider.chat_calls);
    let stream_calls = Arc::clone(&provider.stream_calls);
    let journal = Arc::new(InMemoryJournalStorage::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(crate::ChannelActivitySink::new(tx));

    let mut agent = AgentLoop::new(
        default_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
        Some(sink),
        None,
    );

    let output = agent.run("stream").await.unwrap();
    assert_eq!(output.exit_reason, ExitReason::Complete);
    assert_eq!(chat_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(stream_calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert_eq!(collect_token_texts(&events), vec!["Hel", "lo"]);

    let entries = journal.read_all(&AgentId("test-agent".into())).unwrap();
    let responses: Vec<_> = entries
        .iter()
        .filter_map(|entry| match &entry.entry {
            JournalEntryKind::LlmResponse {
                assistant_message, ..
            } => assistant_message.as_ref(),
            _ => None,
        })
        .collect();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].content, "Hello");
}

#[tokio::test]
async fn provider_tool_call_deltas_map_to_activity_events_without_partial_journal_entries() {
    let provider = StreamingFakeProvider::with_events(
        text_response("done"),
        vec![
            simulacra_types::ProviderStreamEvent::ToolCallDelta {
                index: 0,
                tool_call_id: Some("call-1".into()),
                name: Some("file_read".into()),
                arguments_delta: String::new(),
            },
            simulacra_types::ProviderStreamEvent::ToolCallDelta {
                index: 0,
                tool_call_id: Some("call-1".into()),
                name: Some("file_read".into()),
                arguments_delta: "{\"path\":\"/tmp/a\"}".into(),
            },
            simulacra_types::ProviderStreamEvent::TextDelta {
                text: "done".into(),
            },
        ],
    );
    let journal = Arc::new(InMemoryJournalStorage::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(crate::ChannelActivitySink::new(tx));
    let mut agent = AgentLoop::new(
        default_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
        Some(sink),
        None,
    );

    let output = agent.run("stream tool args").await.unwrap();
    assert_eq!(output.exit_reason, ExitReason::Complete);

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert_eq!(
        collect_tool_call_deltas(&events),
        vec![
            ActivityEvent::ToolCallDelta {
                index: 0,
                tool_call_id: Some("call-1".into()),
                name: Some("file_read".into()),
                arguments_delta: String::new(),
            },
            ActivityEvent::ToolCallDelta {
                index: 0,
                tool_call_id: Some("call-1".into()),
                name: Some("file_read".into()),
                arguments_delta: "{\"path\":\"/tmp/a\"}".into(),
            },
        ]
    );

    let entries = journal.read_all(&AgentId("test-agent".into())).unwrap();
    assert!(entries
        .iter()
        .any(|entry| matches!(entry.entry, JournalEntryKind::LlmResponse { .. })));
    assert!(entries
        .iter()
        .all(|entry| !matches!(entry.entry, JournalEntryKind::ToolCall { .. })));
}

#[tokio::test]
async fn non_streaming_provider_uses_chat_and_emits_single_full_token() {
    let provider = FakeProvider::new(vec![text_response("fallback text")]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(crate::ChannelActivitySink::new(tx));
    let mut agent = AgentLoop::new(
        default_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        Some(sink),
        None,
    );

    let output = agent.run("fallback").await.unwrap();
    assert_eq!(output.exit_reason, ExitReason::Complete);

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert_eq!(collect_token_texts(&events), vec!["fallback text"]);
}

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

#[tokio::test]
async fn cancellation_during_provider_stream_discards_partial_assistant_text() {
    let provider = StreamingFakeProvider::blocking(text_response("partial final"), vec!["partial"]);
    let token = crate::CancellationToken::new(std::time::Duration::from_millis(50));
    let journal = Arc::new(InMemoryJournalStorage::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(crate::ChannelActivitySink::new(tx));
    let mut agent = AgentLoop::new(
        default_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
        Some(sink),
        None,
    );
    agent.set_cancellation_token(token.clone());

    let run = agent.run("cancel stream");
    tokio::pin!(run);

    loop {
        tokio::select! {
            event = rx.recv() => {
                if matches!(event, Some(ActivityEvent::Token { ref text }) if text == "partial") {
                    break;
                }
            }
            result = &mut run => {
                panic!("stream completed before cancellation could be signalled: {result:?}");
            }
        }
    }

    token.signal();
    let output = tokio::time::timeout(std::time::Duration::from_secs(1), run)
        .await
        .expect("stream cancellation should finish promptly")
        .unwrap();

    assert_eq!(output.exit_reason, ExitReason::Cancelled);
    assert!(output
        .messages
        .iter()
        .all(|message| message.content != "partial final"));

    let entries = journal.read_all(&AgentId("test-agent".into())).unwrap();
    assert!(entries
        .iter()
        .any(|entry| matches!(entry.entry, JournalEntryKind::LlmRequest { .. })));
    assert!(entries
        .iter()
        .all(|entry| !matches!(entry.entry, JournalEntryKind::LlmResponse { .. })));
}

#[tokio::test]
async fn cancellation_after_tool_call_delta_does_not_journal_or_append_partial_tool_state() {
    let provider = StreamingFakeProvider::blocking_with_events(
        text_response("partial final"),
        vec![simulacra_types::ProviderStreamEvent::ToolCallDelta {
            index: 0,
            tool_call_id: Some("call-partial".into()),
            name: Some("file_read".into()),
            arguments_delta: "{\"path\":\"/partial\"}".into(),
        }],
    );
    let token = crate::CancellationToken::new(std::time::Duration::from_millis(50));
    let journal = Arc::new(InMemoryJournalStorage::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(crate::ChannelActivitySink::new(tx));
    let mut agent = AgentLoop::new(
        default_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
        Some(sink),
        None,
    );
    agent.set_cancellation_token(token.clone());

    let run = agent.run("cancel stream after tool delta");
    tokio::pin!(run);

    loop {
        tokio::select! {
            event = rx.recv() => {
                if matches!(event, Some(ActivityEvent::ToolCallDelta { ref tool_call_id, .. }) if tool_call_id.as_deref() == Some("call-partial")) {
                    break;
                }
            }
            result = &mut run => {
                panic!("stream completed before cancellation could be signalled: {result:?}");
            }
        }
    }

    token.signal();
    let output = tokio::time::timeout(std::time::Duration::from_secs(1), run)
        .await
        .expect("stream cancellation should finish promptly")
        .unwrap();

    assert_eq!(output.exit_reason, ExitReason::Cancelled);
    assert!(output
        .messages
        .iter()
        .all(|message| message.tool_calls.is_empty()));

    let entries = journal.read_all(&AgentId("test-agent".into())).unwrap();
    assert!(entries
        .iter()
        .all(|entry| !matches!(entry.entry, JournalEntryKind::LlmResponse { .. })));
    assert!(entries
        .iter()
        .all(|entry| !matches!(entry.entry, JournalEntryKind::ToolCall { .. })));
}
