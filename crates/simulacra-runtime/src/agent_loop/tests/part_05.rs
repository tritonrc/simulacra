// -----------------------------------------------------------------------
// S049: Agent turn runtime foundation
// -----------------------------------------------------------------------

fn multi_tool_call_response(calls: Vec<ToolCallMessage>) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: calls,
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens: 20,
            output_tokens: 10,
        },
        finish_reason: FinishReason::ToolUse,
        provider_response_id: Some("resp-tools".into()),
        model: "test-model".into(),
    }
}

fn replay_entry(timestamp_ms: u64, entry: JournalEntryKind) -> JournalEntry {
    JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: AgentId("test-agent".into()),
        timestamp_ms,
        entry,
    }
}

fn conversation(task: &str) -> Vec<Message> {
    vec![
        Message {
            role: Role::System,
            content: "You are a test agent.".into(),
            tool_calls: vec![],
            tool_call_id: None,
        },
        Message {
            role: Role::User,
            content: task.into(),
            tool_calls: vec![],
            tool_call_id: None,
        },
    ]
}

#[test]
fn turn_runtime_types_track_step_and_cancellation_state() {
    let messages = conversation("snapshot");
    let tool_defs = vec![ToolDefinition {
        name: "echo".into(),
        description: "Echoes input".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }];
    let step = StepContext::new(messages.clone(), tool_defs.clone());

    assert_eq!(step.messages().len(), messages.len());
    assert_eq!(step.messages()[0].role, Role::System);
    assert_eq!(step.messages()[1].content, "snapshot");
    assert_eq!(step.tool_definitions().len(), 1);
    assert_eq!(step.tool_definitions()[0].name, "echo");

    let cancellation = crate::CancellationToken::new(std::time::Duration::from_millis(50));
    let context = TurnContext::new(
        AgentId("test-agent".into()),
        "test-model".into(),
        CapabilityToken::default(),
        Some(cancellation.clone()),
    );
    let active = ActiveTurn::new(context);

    assert_eq!(active.context().agent_id(), &AgentId("test-agent".into()));
    assert_eq!(active.context().model(), "test-model");
    assert!(!active.state().cancelled);

    active.record_tool_call();
    active.mark_cancelled();

    let state = active.state();
    assert_eq!(state.tool_call_count, 1);
    assert!(state.cancelled);
    assert!(active.context().cancellation().is_some());
}

struct CapturingProvider {
    response: ProviderResponse,
    captured_messages: Arc<Mutex<Option<Vec<Message>>>>,
    captured_tools: Arc<Mutex<Option<Vec<ToolDefinition>>>>,
}

impl Provider for CapturingProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        *self.captured_messages.lock().unwrap() = Some(messages.to_vec());
        *self.captured_tools.lock().unwrap() = Some(tools.to_vec());
        let response = self.response.clone();
        Box::pin(async move { Ok(response) })
    }
}

#[tokio::test]
async fn agent_loop_uses_step_context_for_provider_input() {
    let captured_messages = Arc::new(Mutex::new(None));
    let captured_tools = Arc::new(Mutex::new(None));
    let provider = CapturingProvider {
        response: text_response("captured"),
        captured_messages: Arc::clone(&captured_messages),
        captured_tools: Arc::clone(&captured_tools),
    };
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool)).unwrap();
    let mut agent = AgentLoop::new(
        default_config(),
        Box::new(provider),
        tools,
        Box::new(TruncatingContext { keep_recent: 1 }),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        None,
        None,
    );

    let output = agent.run("capture me").await.unwrap();
    assert_eq!(output.exit_reason, ExitReason::Complete);

    let messages = captured_messages
        .lock()
        .unwrap()
        .take()
        .expect("provider should receive compacted messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, Role::System);
    assert_eq!(messages[1].content, "capture me");

    let tools = captured_tools
        .lock()
        .unwrap()
        .take()
        .expect("provider should receive tool definitions");
    assert_eq!(tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), vec!["echo"]);
}

struct CountingProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl Provider for CountingProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(text_response("should not be called")) })
    }
}

#[tokio::test]
async fn cancellation_before_provider_returns_cancelled_without_provider_call() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let token = crate::CancellationToken::new(std::time::Duration::from_millis(50));
    token.signal();

    let mut agent = AgentLoop::new(
        default_config(),
        Box::new(CountingProvider {
            calls: Arc::clone(&calls),
        }),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        None,
        None,
    );
    agent.set_cancellation_token(token);

    let output = agent.run("cancel before provider").await.unwrap();

    assert_eq!(output.exit_reason, ExitReason::Cancelled);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

struct ParallelBarrierTool {
    name: &'static str,
    barrier: Arc<tokio::sync::Barrier>,
}

impl simulacra_types::Tool for ParallelBarrierTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "Waits at a barrier before returning".into(),
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
        let barrier = Arc::clone(&self.barrier);
        let name = self.name;
        Box::pin(async move {
            barrier.wait().await;
            Ok(serde_json::json!({ "tool": name }))
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn all_parallel_tool_batches_overlap_and_preserve_provider_order() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(ParallelBarrierTool {
            name: "parallel_a",
            barrier: Arc::clone(&barrier),
        }))
        .unwrap();
    tools
        .register(Box::new(ParallelBarrierTool {
            name: "parallel_b",
            barrier,
        }))
        .unwrap();
    let provider = FakeProvider::new(vec![
        multi_tool_call_response(vec![
            ToolCallMessage {
                id: "tc-a".into(),
                name: "parallel_a".into(),
                arguments: serde_json::json!({}),
            },
            ToolCallMessage {
                id: "tc-b".into(),
                name: "parallel_b".into(),
                arguments: serde_json::json!({}),
            },
        ]),
        text_response("done"),
    ]);
    let mut agent = build_loop(
        provider,
        tools,
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
    );

    let output = tokio::time::timeout(std::time::Duration::from_secs(1), agent.run("parallel"))
        .await
        .expect("parallel-capable tools should overlap instead of deadlocking")
        .unwrap();

    assert_eq!(output.exit_reason, ExitReason::Complete);
    let tool_call_ids = output
        .messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| message.tool_call_id.as_deref().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(tool_call_ids, vec!["tc-a", "tc-b"]);
}

#[tokio::test]
async fn replay_tool_batches_use_recorded_serial_results_even_when_tools_are_parallel_capable() {
    let assistant = multi_tool_call_response(vec![
        ToolCallMessage {
            id: "tc-a".into(),
            name: "parallel_a".into(),
            arguments: serde_json::json!({}),
        },
        ToolCallMessage {
            id: "tc-b".into(),
            name: "parallel_b".into(),
            arguments: serde_json::json!({}),
        },
    ])
    .message;
    let replay_entries = vec![
        replay_entry(1, JournalEntryKind::TurnStart),
        replay_entry(
            2,
            JournalEntryKind::LlmRequest {
                model: "test-model".into(),
                message_count: 2,
            },
        ),
        replay_entry(
            3,
            JournalEntryKind::LlmResponse {
                model: "test-model".into(),
                token_usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                finish_reason: "ToolUse".into(),
                assistant_message: Some(assistant),
            },
        ),
        replay_entry(
            4,
            JournalEntryKind::ToolCall {
                tool_call_id: Some("tc-a".into()),
                tool_name: "parallel_a".into(),
                arguments: serde_json::json!({}),
            },
        ),
        replay_entry(
            5,
            JournalEntryKind::ToolResult {
                tool_call_id: Some("tc-a".into()),
                tool_name: "parallel_a".into(),
                content: "replayed a".into(),
                is_error: false,
            },
        ),
        replay_entry(
            6,
            JournalEntryKind::ToolCall {
                tool_call_id: Some("tc-b".into()),
                tool_name: "parallel_b".into(),
                arguments: serde_json::json!({}),
            },
        ),
        replay_entry(
            7,
            JournalEntryKind::ToolResult {
                tool_call_id: Some("tc-b".into()),
                tool_name: "parallel_b".into(),
                content: "replayed b".into(),
                is_error: false,
            },
        ),
    ];
    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(FakeProvider::new(vec![text_response("done")])),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        Box::new(FixedClock(123)),
        Some(replay_entries),
    );

    let output = agent.run("replay").await.unwrap();

    assert_eq!(output.exit_reason, ExitReason::Complete);
    let tool_contents = output
        .messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>();
    assert_eq!(tool_contents, vec!["replayed a", "replayed b"]);
}

struct CancellableTool {
    name: &'static str,
    started: Arc<tokio::sync::Notify>,
}

impl simulacra_types::Tool for CancellableTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "Blocks until runtime cancellation aborts it".into(),
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
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            started.notify_one();
            std::future::pending::<Result<serde_json::Value, simulacra_types::ToolError>>().await
        })
    }
}

#[tokio::test]
async fn cancellation_during_non_waiting_tool_returns_cancelled_error_result() {
    let started = Arc::new(tokio::sync::Notify::new());
    let token = crate::CancellationToken::new(std::time::Duration::from_millis(50));
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(CancellableTool {
            name: "cancellable",
            started: Arc::clone(&started),
        }))
        .unwrap();
    let mut agent = build_loop(
        FakeProvider::new(vec![tool_call_response(
            "cancellable",
            serde_json::json!({}),
        )]),
        tools,
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
    );
    agent.set_cancellation_token(token.clone());

    let handle = tokio::spawn(async move {
        let mut messages = conversation("cancel tool");
        let result = agent.run_single_turn(&mut messages).await.unwrap();
        (result, messages)
    });

    started.notified().await;
    token.signal();
    let (result, messages) =
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("cancelled tool should not hang")
            .unwrap();

    match result {
        TurnResult::ToolCallsProcessed { tool_results, .. } => {
            assert_eq!(tool_results.len(), 1);
            assert_eq!(tool_results[0].content, "ERROR: cancelled by user");
        }
        other => panic!("expected processed tool call, got {other:?}"),
    }
    assert!(messages
        .iter()
        .any(|message| message.content == "ERROR: cancelled by user"));
}

struct CleanupWaitingTool {
    started: Arc<tokio::sync::Notify>,
    finish_cleanup: Arc<tokio::sync::Notify>,
}

impl simulacra_types::Tool for CleanupWaitingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "cleanup_waiter".into(),
            description: "Finishes only after cleanup is released".into(),
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
        let started = Arc::clone(&self.started);
        let finish_cleanup = Arc::clone(&self.finish_cleanup);
        Box::pin(async move {
            started.notify_one();
            finish_cleanup.notified().await;
            Ok(serde_json::json!({ "cleanup": "finished" }))
        })
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn cancellation_during_waiting_tool_waits_for_cleanup_before_cancelled_result() {
    let started = Arc::new(tokio::sync::Notify::new());
    let finish_cleanup = Arc::new(tokio::sync::Notify::new());
    let token = crate::CancellationToken::new(std::time::Duration::from_millis(50));
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(CleanupWaitingTool {
            started: Arc::clone(&started),
            finish_cleanup: Arc::clone(&finish_cleanup),
        }))
        .unwrap();
    let mut agent = build_loop(
        FakeProvider::new(vec![tool_call_response(
            "cleanup_waiter",
            serde_json::json!({}),
        )]),
        tools,
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
    );
    agent.set_cancellation_token(token.clone());

    let handle = tokio::spawn(async move {
        let mut messages = conversation("wait for cleanup");
        let result = agent.run_single_turn(&mut messages).await.unwrap();
        (result, messages)
    });

    started.notified().await;
    token.signal();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !handle.is_finished(),
        "waiting tools should be allowed to finish cleanup after cancellation"
    );

    finish_cleanup.notify_waiters();
    let (result, messages) =
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("cleanup-waiting cancelled tool should finish once cleanup completes")
            .unwrap();

    match result {
        TurnResult::ToolCallsProcessed { tool_results, .. } => {
            assert_eq!(tool_results.len(), 1);
            assert_eq!(tool_results[0].content, "ERROR: cancelled by user");
        }
        other => panic!("expected processed tool call, got {other:?}"),
    }
    assert!(messages
        .iter()
        .any(|message| message.content == "ERROR: cancelled by user"));
}
