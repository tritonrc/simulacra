    #[tokio::test]
    async fn budget_exhaustion_is_logged_at_warn_with_resource_usage_and_limit() {
        let (subscriber, _spans, captured_events) = setup_capture();
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![]);

        let mut budget = default_budget();
        budget.max_turns = 1;
        budget.used_turns = 1;

        let mut agent = build_loop(
            provider,
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal,
            budget,
        );

        let _guard = tracing::subscriber::set_default(subscriber);
        let result = agent.run("trip the turn budget").await;
        assert!(result.is_err(), "an exhausted budget should stop the loop");

        let events = captured_events.lock().unwrap();
        let exhaustion_event = events
            .iter()
            .find(|event| {
                event.level == "WARN"
                    && event.fields.get("simulacra.agent.budget.resource")
                        == Some(&"turns".to_string())
                    && event.fields.get("simulacra.agent.budget.used") == Some(&"1".to_string())
                    && event.fields.get("simulacra.agent.budget.limit") == Some(&"1".to_string())
            })
            .expect("expected a WARN event with the exhausted resource, used value, and limit");

        assert_eq!(
            exhaustion_event.current_span.as_deref(),
            Some("invoke_agent")
        );
    }

    #[tokio::test]
    async fn budget_remaining_gauge_is_updated_after_each_budget_consuming_operation() {
        let (subscriber, _spans, captured_events) = setup_capture();
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![
            tool_call_response("echo", serde_json::json!({"msg": "hi"})),
            text_response("Done!"),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(EchoTool));

        let mut agent = build_loop(
            provider,
            tools,
            Box::new(PassthroughContext),
            journal,
            default_budget(),
        );

        let _guard = tracing::subscriber::set_default(subscriber);
        let _output = agent
            .run("consume budget twice")
            .await
            .expect("run should succeed");

        let events = captured_events.lock().unwrap();
        let gauge_updates = events
            .iter()
            .filter(|event| {
                event
                    .fields
                    .contains_key("simulacra.agent.budget.remaining")
            })
            .count();

        assert!(
            gauge_updates >= 2,
            "expected simulacra.agent.budget.remaining to be updated after each budget-consuming operation"
        );
    }

    #[tokio::test]
    async fn budget_check_failures_emit_current_span_event_with_exhaustion_details() {
        let (subscriber, captured_spans, captured_events) = setup_capture();
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![]);

        let mut budget = default_budget();
        budget.max_turns = 1;
        budget.used_turns = 1;

        let mut agent = build_loop(
            provider,
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal,
            budget,
        );

        let _guard = tracing::subscriber::set_default(subscriber);
        let result = agent.run("emit a budget exhaustion event").await;
        assert!(result.is_err(), "an exhausted budget should fail the run");

        let spans = captured_spans.lock().unwrap();
        assert!(
            spans.iter().any(|span| {
                span.name == "invoke_agent"
                    && span.fields.get("gen_ai.agent.name") == Some(&"test-agent".to_string())
            }),
            "expected the invoke_agent span to be active while the budget failure is emitted"
        );

        let events = captured_events.lock().unwrap();
        let exhaustion_event = events
            .iter()
            .find(|event| {
                event.current_span.as_deref() == Some("invoke_agent")
                    && event
                        .fields
                        .get("message")
                        .is_some_and(|message| message.contains("budget exhausted"))
                    && event.fields.get("simulacra.agent.budget.resource")
                        == Some(&"turns".to_string())
                    && event.fields.get("simulacra.agent.budget.used") == Some(&"1".to_string())
                    && event.fields.get("simulacra.agent.budget.limit") == Some(&"1".to_string())
            })
            .expect("expected a current-span budget exhaustion event with detailed fields");

        assert_eq!(
            exhaustion_event.current_span.as_deref(),
            Some("invoke_agent")
        );
    }

    #[tokio::test]
    async fn replay_divergence_is_logged_at_error() {
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
                panic!("Provider::chat should not be called for a divergence test");
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
                entry: JournalEntryKind::ToolResult {
                    tool_call_id: Some("tc-1".into()),
                    tool_name: "echo".into(),
                    content: "wrong entry kind".into(),
                    is_error: false,
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
            Box::new(FixedClock(2_200)),
            Some(replay_entries),
        );

        let _guard = tracing::subscriber::set_default(subscriber);
        let result = agent.run("hit replay divergence").await;
        assert!(result.is_err(), "divergent replay should fail loudly");

        let events = captured_events.lock().unwrap();
        let divergence_event = events
            .iter()
            .find(|event| {
                event.level == "ERROR"
                    && event
                        .fields
                        .values()
                        .any(|value| value.contains("LlmResponse"))
                    && event
                        .fields
                        .values()
                        .any(|value| value.contains("ToolResult"))
            })
            .expect("expected an ERROR log describing the replay divergence");

        assert_eq!(divergence_event.level, "ERROR");
    }

    #[tokio::test]
    async fn invoke_agent_span_contains_child_llm_span() {
        // Edge case: the invoke_agent span should wrap the full run so provider chat spans are
        // nested underneath it instead of being emitted as detached top-level spans.
        #[derive(Debug, Clone)]
        struct SpanRelationship {
            name: String,
            parent: Option<String>,
        }

        struct ParentCaptureLayer {
            spans: Arc<StdMutex<Vec<SpanRelationship>>>,
        }

        impl<S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>
            tracing_subscriber::Layer<S> for ParentCaptureLayer
        {
            fn on_new_span(
                &self,
                attrs: &tracing::span::Attributes<'_>,
                _id: &tracing::span::Id,
                ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let parent = attrs
                    .parent()
                    .and_then(|parent_id| ctx.span(parent_id))
                    .map(|span| span.name().to_string())
                    .or_else(|| {
                        if attrs.is_contextual() {
                            ctx.current_span()
                                .id()
                                .and_then(|parent_id| ctx.span(parent_id))
                                .map(|span| span.name().to_string())
                        } else {
                            None
                        }
                    });

                self.spans.lock().unwrap().push(SpanRelationship {
                    name: attrs.metadata().name().to_string(),
                    parent,
                });
            }
        }

        struct InstrumentedProvider;

        impl Provider for InstrumentedProvider {
            fn chat<'a>(
                &'a self,
                _messages: &'a [Message],
                _tools: &'a [ToolDefinition],
                _budget: &'a mut ResourceBudget,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async move {
                    let _chat_span = tracing::info_span!(
                        "chat",
                        "gen_ai.operation.name" = "chat",
                        "gen_ai.request.model" = "instrumented-test-model",
                        "gen_ai.provider.name" = "fake",
                    )
                    .entered();
                    Ok(text_response("nested span"))
                })
            }
        }

        let spans = Arc::new(StdMutex::new(Vec::new()));
        let subscriber =
            tracing_subscriber::registry::Registry::default().with(ParentCaptureLayer {
                spans: Arc::clone(&spans),
            });
        let journal = Arc::new(InMemoryJournalStorage::new());
        let mut agent = AgentLoop::new(
            default_config(),
            Box::new(InstrumentedProvider),
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal,
            default_budget(),
            None,
            None,
        );

        let _guard = tracing::subscriber::set_default(subscriber);
        let _ = agent.run("nest spans").await.unwrap();

        let spans = spans.lock().unwrap();
        assert!(spans.iter().any(|span| span.name == "invoke_agent"));
        assert!(
            spans.iter().any(|span| {
                span.name == "chat" && span.parent.as_deref() == Some("invoke_agent")
            })
        );
    }
