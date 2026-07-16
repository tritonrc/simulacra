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
async fn mcp_meta_tool_stream_deltas_redact_arguments_while_ordinary_tools_remain_unchanged() {
    let provider = StreamingFakeProvider::with_events(
        text_response("done"),
        vec![
            simulacra_types::ProviderStreamEvent::ToolCallDelta {
                index: 0,
                tool_call_id: Some("search-1".into()),
                name: Some("mcp_search".into()),
                arguments_delta: "{\"query\":\"QUERYSECRET".into(),
            },
            simulacra_types::ProviderStreamEvent::ToolCallDelta {
                index: 0,
                tool_call_id: Some("search-1".into()),
                name: None,
                arguments_delta: " Authorization: Bearer SEARCHAUTH\"}".into(),
            },
            simulacra_types::ProviderStreamEvent::ToolCallDelta {
                index: 1,
                tool_call_id: Some("call-1".into()),
                name: Some("mcp_call".into()),
                arguments_delta: "{\"arguments\":{\"token\":\"CALLSECRET\"}}".into(),
            },
            simulacra_types::ProviderStreamEvent::ToolCallDelta {
                index: 2,
                tool_call_id: Some("ordinary-1".into()),
                name: Some("file_read".into()),
                arguments_delta: "{\"path\":\"/tmp/kept\"}".into(),
            },
        ],
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut agent = AgentLoop::new(
        default_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        Some(Arc::new(crate::ChannelActivitySink::new(tx))),
        None,
    );

    agent.run("stream secret tool args").await.unwrap();
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let deltas = collect_tool_call_deltas(&events);
    assert_eq!(deltas.len(), 4);
    for delta in &deltas[..3] {
        let ActivityEvent::ToolCallDelta {
            tool_call_id,
            name,
            arguments_delta,
            ..
        } = delta
        else {
            unreachable!()
        };
        assert!(tool_call_id.is_some());
        assert!(name.as_deref().is_some_and(|name| name.starts_with("mcp_")) || name.is_none());
        assert_eq!(arguments_delta, "[REDACTED]");
    }
    assert!(matches!(
        &deltas[3],
        ActivityEvent::ToolCallDelta { name: Some(name), arguments_delta, .. }
            if name == "file_read" && arguments_delta == "{\"path\":\"/tmp/kept\"}"
    ));
    let rendered = format!("{deltas:?}");
    assert!(!rendered.contains("QUERYSECRET"));
    assert!(!rendered.contains("SEARCHAUTH"));
    assert!(!rendered.contains("CALLSECRET"));
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
