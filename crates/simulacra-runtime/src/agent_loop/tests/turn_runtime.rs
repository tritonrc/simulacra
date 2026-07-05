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
            provider_content: vec![],
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
            provider_content: vec![],
        },
        Message {
            role: Role::User,
            content: task.into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
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
    assert_eq!(
        tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(),
        vec!["echo"]
    );
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
