    #[tokio::test]
    async fn journal_entries_counter_tracks_entries_by_kind() {
        let (subscriber, _spans, captured_events) = setup_capture();
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![text_response("count journal entries")]);
        let mut agent = build_loop(
            provider,
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal,
            default_budget(),
        );

        let _guard = tracing::subscriber::set_default(subscriber);
        let _ = agent.run("emit journal metrics").await.unwrap();

        let events = captured_events.lock().unwrap();
        assert!(
            events.iter().any(|event| {
                event.fields.get("simulacra.journal.entries") == Some(&"1".to_string())
                    && event.fields.get("simulacra.journal.entry_kind")
                        == Some(&"TurnStart".to_string())
            }),
            "expected simulacra.journal.entries counter updates tagged by entry kind"
        );
    }

    #[tokio::test]
    async fn journal_replay_ratio_gauge_reports_fraction_replayed() {
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

        let (subscriber, _spans, captured_events) = setup_capture();
        let journal = Arc::new(InMemoryJournalStorage::new());
        let replay_entries = vec![
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 1000,
                entry: JournalEntryKind::TurnStart,
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 1001,
                entry: JournalEntryKind::LlmRequest {
                    model: "test-model".into(),
                    message_count: 2,
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
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
                        content: "fully replayed".into(),
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
            journal,
            default_budget(),
            Box::new(FixedClock(2_100)),
            Some(replay_entries),
        );

        let _guard = tracing::subscriber::set_default(subscriber);
        let _ = agent.run("measure replay ratio").await.unwrap();

        let events = captured_events.lock().unwrap();
        assert!(
            events.iter().any(|event| {
                event
                    .fields
                    .get("simulacra.journal.replay.ratio")
                    .is_some_and(|value| value == "1" || value == "1.0")
            }),
            "expected simulacra.journal.replay.ratio gauge to report the replay fraction"
        );
    }

    #[tokio::test]
    async fn capability_denials_emit_warn_event_on_current_span() {
        let (subscriber, captured_spans, captured_events) = setup_capture();
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![
            tool_call_response("deny_shell", serde_json::json!({})),
            text_response("done"),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(DenyShellTool));

        let mut agent = build_loop(
            provider,
            tools,
            Box::new(PassthroughContext),
            journal,
            default_budget(),
        );

        let _guard = tracing::subscriber::set_default(subscriber);
        let _ = agent.run("emit a capability denial").await.unwrap();

        let spans = captured_spans.lock().unwrap();
        let agent_span = spans
            .iter()
            .find(|span| {
                span.name == "invoke_agent"
                    && span.fields.get("gen_ai.agent.name") == Some(&"test-agent".to_string())
            })
            .expect("expected an invoke_agent span with the agent name");

        let events = captured_events.lock().unwrap();
        let denial_event = events
            .iter()
            .find(|event| {
                event.level == "WARN"
                    && event.current_span.as_deref() == Some("invoke_agent")
                    && event.fields.get("simulacra.capability.operation")
                        == Some(&"shell".to_string())
                    && event.fields.get("simulacra.capability.reason")
                        == Some(&"shell capability not granted".to_string())
            })
            .expect("expected a WARN capability denial event on the invoke_agent span");

        assert_eq!(
            agent_span.fields.get("gen_ai.agent.name"),
            Some(&"test-agent".to_string())
        );
        assert_eq!(
            denial_event.fields.get("simulacra.capability.denials"),
            Some(&"1".to_string()),
            "capability denials should increment the simulacra.capability.denials counter"
        );
    }

    #[tokio::test]
    async fn capability_denial_warn_event_includes_agent_name_for_attribution() {
        let (subscriber, _captured_spans, captured_events) = setup_capture();
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![
            tool_call_response("deny_shell", serde_json::json!({})),
            text_response("done"),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(DenyShellTool));

        let mut agent = build_loop(
            provider,
            tools,
            Box::new(PassthroughContext),
            journal,
            default_budget(),
        );

        let _guard = tracing::subscriber::set_default(subscriber);
        let _ = agent.run("emit a capability denial").await.unwrap();

        let events = captured_events.lock().unwrap();
        let denial_event = events
            .iter()
            .find(|event| {
                event.level == "WARN"
                    && event.current_span.as_deref() == Some("invoke_agent")
                    && event.fields.get("simulacra.capability.operation")
                        == Some(&"shell".to_string())
            })
            .expect("expected a WARN capability denial event on the invoke_agent span");

        assert_eq!(
            denial_event.fields.get("gen_ai.agent.name"),
            Some(&"test-agent".to_string()),
            "capability denial WARN event should include the agent name for attribution"
        );
    }

