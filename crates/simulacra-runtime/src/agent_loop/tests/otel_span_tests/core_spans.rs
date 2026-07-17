    #[tokio::test]
    async fn agent_loop_emits_invoke_agent_span() {
        let (subscriber, captured_spans, _events) = setup_capture();
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![text_response("Hello!")]);
        let mut agent = build_loop(
            provider,
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal,
            default_budget(),
        );

        let (_capture_guard, _guard) = install_capture(subscriber).await;
        let _output = agent.run("Say hello").await.expect("run should succeed");

        let spans = captured_spans.lock().unwrap();
        let agent_span = spans
            .iter()
            .find(|s| s.fields.get("gen_ai.operation.name") == Some(&"invoke_agent".to_string()))
            .expect("expected a span with gen_ai.operation.name=invoke_agent");

        assert_eq!(
            agent_span.fields.get("gen_ai.agent.name"),
            Some(&"test-agent".to_string())
        );
    }

    // S010: Tool call events emit gen_ai.tool.message
    #[tokio::test]
    async fn tool_calls_emit_gen_ai_tool_message_event() {
        let (subscriber, _spans, captured_events) = setup_capture();
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![
            tool_call_response("echo", serde_json::json!({"msg": "hi"})),
            text_response("Done!"),
        ]);
        let mut tools = ToolRegistry::new();
        tools
            .register(Box::new(EchoTool))
            .expect("test tool registration should succeed");

        let mut agent = build_loop(
            provider,
            tools,
            Box::new(PassthroughContext),
            journal,
            default_budget(),
        );

        let (_capture_guard, _guard) = install_capture(subscriber).await;
        let _output = agent
            .run("Use echo tool")
            .await
            .expect("run should succeed");

        let events = captured_events.lock().unwrap();
        let tool_event = events
            .iter()
            .find(|e| e.fields.contains_key("gen_ai.tool.message"))
            .expect("expected an event with gen_ai.tool.message field");

        // Verify the tool name is in the event
        assert!(
            tool_event
                .fields
                .get("gen_ai.tool.message")
                .unwrap()
                .contains("echo"),
            "tool event should reference the tool name"
        );
    }

    struct SecretMcpResultTool;

    impl simulacra_types::Tool for SecretMcpResultTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "mcp_call".into(),
                description: "secret MCP result fixture".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }
        }

        fn call(
            &self,
            _arguments: serde_json::Value,
            _capability: &CapabilityToken,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>> + Send + '_>> {
            Box::pin(async {
                Ok(simulacra_types::ToolOutput::success(
                    "https://RESULTUSER:RESULTPASS@example.invalid/mcp?token=RESULTTOKEN Authorization: Bearer RESULTAUTH /private/RESULTMODULE.wasm",
                ).to_value())
            })
        }
    }

    #[tokio::test]
    async fn mcp_call_result_reaches_model_unchanged_but_registry_event_logs_only_safe_metadata() {
        let (subscriber, _spans, captured_events) = setup_capture();
        let provider = FakeProvider::new(vec![
            tool_call_response("mcp_call", serde_json::json!({"server":"github","tool":"issues","arguments":{}})),
            text_response("Done"),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(SecretMcpResultTool)).unwrap();
        let mut agent = build_loop(
            provider,
            tools,
            Box::new(PassthroughContext),
            Arc::new(InMemoryJournalStorage::new()),
            default_budget(),
        );
        let (_capture_guard, _guard) = install_capture(subscriber).await;
        let output = agent.run("call MCP").await.unwrap();
        let model_tool_message = output.messages.iter().find(|message| message.role == Role::Tool).expect("model tool message");
        assert!(model_tool_message.content.contains("RESULTUSER"));
        assert!(model_tool_message.content.contains("RESULTAUTH"));

        let events = captured_events.lock().unwrap();
        let result_event = events.iter().find(|event| {
            event.fields.get("gen_ai.tool.name") == Some(&"mcp_call".into())
                && event.fields.contains_key("gen_ai.tool.result_length")
        }).expect("mcp_call result event");
        assert_eq!(result_event.fields.get("gen_ai.tool.message"), Some(&"[REDACTED]".into()));
        assert!(result_event
            .fields
            .contains_key("gen_ai.tool.result_length"));
        let rendered = format!("{result_event:?}");
        for secret in ["RESULTUSER", "RESULTPASS", "RESULTTOKEN", "RESULTAUTH", "RESULTMODULE"] {
            assert!(!rendered.contains(secret));
        }
    }

    #[tokio::test]
    async fn journal_append_span_records_entry_kind_and_live_mode() {
        let (subscriber, captured_spans, _events) = setup_capture();
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![text_response("hello from live mode")]);
        let mut agent = build_loop(
            provider,
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal,
            default_budget(),
        );

        let (_capture_guard, _guard) = install_capture(subscriber).await;
        let _ = agent.run("capture live journal spans").await.unwrap();

        let spans = captured_spans.lock().unwrap();
        let journal_span = spans
            .iter()
            .find(|span| {
                span.fields.get("simulacra.operation.name") == Some(&"journal_append".to_string())
                    && span.fields.get("simulacra.journal.entry_kind")
                        == Some(&"TurnStart".to_string())
            })
            .expect("expected a journal_append span for a TurnStart entry");

        assert_eq!(
            journal_span.fields.get("simulacra.journal.mode"),
            Some(&"live".to_string()),
            "live journal appends should be tagged with simulacra.journal.mode=live"
        );
    }

    #[tokio::test]
    async fn replayed_journal_entries_are_tagged_replayed() {
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

        let (subscriber, captured_spans, _events) = setup_capture();
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
                        content: "replayed answer".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
            provider_content: vec![],
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
            Box::new(FixedClock(2_000)),
            Some(replay_entries),
        );

        let (_capture_guard, _guard) = install_capture(subscriber).await;
        let _ = agent.run("replayed task").await.unwrap();

        let spans = captured_spans.lock().unwrap();
        assert!(
            spans.iter().any(|span| {
                span.fields.get("simulacra.operation.name") == Some(&"journal_append".to_string())
                    && span.fields.get("simulacra.journal.mode") == Some(&"replayed".to_string())
            }),
            "expected replayed journal appends to be tagged with simulacra.journal.mode=replayed"
        );
    }
