#[tokio::test]
async fn injectable_clock_produces_deterministic_timestamps() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![text_response("Hello!")]);

    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
        Box::new(FixedClock(42_000)),
        None,
    );

    let _ = agent.run("test").await.expect("run should succeed");

    let entries = journal
        .read_all(&AgentId("test-agent".into()))
        .expect("read_all should succeed");
    for entry in &entries {
        assert_eq!(entry.timestamp_ms, 42_000);
    }
}

#[tokio::test]
async fn replay_with_recorded_llm_response_skips_provider() {
    let journal = Arc::new(InMemoryJournalStorage::new());

    struct PanickingProvider;

    impl Provider for PanickingProvider {
        fn chat<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a [ToolDefinition],
            _budget: &'a mut ResourceBudget,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<ProviderResponse, simulacra_types::ProviderError>,
                    > + Send
                    + 'a,
            >,
        > {
            panic!("Provider::chat should not be called during replay");
        }
    }

    let agent_id = AgentId("test-agent".into());
    let replay_entries = vec![
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 1000,
            entry: JournalEntryKind::TurnStart,
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 1001,
            entry: JournalEntryKind::LlmRequest {
                model: "test-model".into(),
                message_count: 2,
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 1002,
            entry: JournalEntryKind::LlmResponse {
                model: "test-model".into(),
                token_usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
                finish_reason: "EndTurn".into(),
                assistant_message: Some(Message {
                    role: Role::Assistant,
                    content: "Replayed answer".into(),
                    tool_calls: vec![],
                    tool_call_id: None,
                }),
            },
        },
    ];

    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(PanickingProvider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
        Box::new(FixedClock(2000)),
        Some(replay_entries),
    );

    let output = agent
        .run("replayed task")
        .await
        .expect("replay should succeed");

    assert_eq!(output.exit_reason, ExitReason::Complete);
    assert_eq!(output.token_usage.input_tokens, 10);
    assert_eq!(output.token_usage.output_tokens, 5);
}
