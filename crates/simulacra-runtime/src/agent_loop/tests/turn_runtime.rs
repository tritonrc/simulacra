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
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
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

// -----------------------------------------------------------------------
// Provider content blocks on tool results
// -----------------------------------------------------------------------

/// Blocks a tool attaches to its result: two Anthropic image blocks around an
/// opaque block from another provider. The runtime must carry all three
/// untouched and in order; it does not inspect any block's value.
fn runtime_provider_blocks() -> Vec<simulacra_types::ProviderContentBlock> {
    vec![
        simulacra_types::ProviderContentBlock {
            provider: "anthropic".into(),
            value: serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "runtime-first"
                }
            }),
        },
        simulacra_types::ProviderContentBlock {
            provider: "other-provider".into(),
            value: serde_json::json!({
                "kind": "opaque",
                "nested": { "keep": [1, 2, 3], "text": "not inspected" }
            }),
        },
        simulacra_types::ProviderContentBlock {
            provider: "anthropic".into(),
            value: serde_json::json!({
                "type": "image",
                "source": {
                    "type": "url",
                    "url": "https://example.test/runtime-second.png"
                }
            }),
        },
    ]
}

/// A raw tool value in the typed output shape. `EchoTool` returns its
/// arguments verbatim, so handing it this value makes the registry's
/// `output_from_value` conversion the path under test.
fn typed_echo_arguments(
    content: &str,
    provider_content: &[simulacra_types::ProviderContentBlock],
) -> serde_json::Value {
    serde_json::json!({
        "content": content,
        "is_error": false,
        "log_preview": content,
        "provider_content": provider_content,
    })
}

fn tool_message<'a>(messages: &'a [Message], tool_call_id: &str) -> &'a Message {
    messages
        .iter()
        .find(|message| {
            message.role == Role::Tool && message.tool_call_id.as_deref() == Some(tool_call_id)
        })
        .unwrap_or_else(|| panic!("expected a tool message for {tool_call_id}"))
}

fn assert_tool_message_has_no_provider_content(messages: &[Message], content: &str) {
    let message = messages
        .iter()
        .find(|message| message.role == Role::Tool && message.content == content)
        .unwrap_or_else(|| panic!("expected a tool message with content {content:?}"));
    assert_eq!(
        message.provider_content,
        Vec::<simulacra_types::ProviderContentBlock>::new()
    );
}

#[tokio::test]
async fn live_tool_result_message_and_journal_preserve_provider_blocks() {
    let expected_blocks = runtime_provider_blocks();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(EchoTool))
        .expect("echo tool registration should succeed");
    let mut agent = build_loop(
        FakeProvider::new(vec![tool_call_response(
            "echo",
            typed_echo_arguments("image attached", &expected_blocks),
        )]),
        tools,
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
    );
    let mut messages = conversation("capture tool result blocks");

    let result = agent
        .run_single_turn(&mut messages)
        .await
        .expect("tool turn should succeed");

    assert!(matches!(result, TurnResult::ToolCallsProcessed { .. }));
    let message = tool_message(&messages, "tc-1");
    assert_eq!(message.content, "image attached");
    assert_eq!(message.provider_content, expected_blocks);

    let entries = journal
        .read_all(&AgentId("test-agent".into()))
        .expect("journal entries should be readable");
    let recorded = entries
        .iter()
        .find_map(|entry| match &entry.entry {
            JournalEntryKind::ToolResult {
                tool_call_id: Some(id),
                tool_name,
                content,
                is_error,
                provider_content,
            } if id == "tc-1" && tool_name == "echo" => {
                Some((content.clone(), *is_error, provider_content.clone()))
            }
            _ => None,
        })
        .expect("tool result should be journaled");

    assert_eq!(recorded.0, "image attached");
    assert!(!recorded.1);
    assert_eq!(recorded.2, expected_blocks);
}

#[tokio::test]
async fn capability_denial_tool_message_has_empty_provider_content() {
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(DenyShellTool))
        .expect("deny shell tool should register");
    let mut agent = build_loop(
        FakeProvider::new(vec![tool_call_response(
            "deny_shell",
            serde_json::json!({}),
        )]),
        tools,
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
    );
    let mut messages = conversation("deny shell");

    agent
        .run_single_turn(&mut messages)
        .await
        .expect("capability denial should still produce a tool result");

    assert_tool_message_has_no_provider_content(
        &messages,
        "ERROR: capability denied: capability denied: shell — shell capability not granted",
    );
}

#[tokio::test]
async fn execution_error_tool_message_has_empty_provider_content() {
    // No tool is registered, so the registry fails the call with an
    // execution error before any tool output exists.
    let mut agent = build_loop(
        FakeProvider::new(vec![tool_call_response(
            "missing_tool",
            serde_json::json!({}),
        )]),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
    );
    let mut messages = conversation("call a missing tool");

    agent
        .run_single_turn(&mut messages)
        .await
        .expect("execution failure should still produce a tool result");

    assert_tool_message_has_no_provider_content(
        &messages,
        "ERROR: execution failed: unknown tool: missing_tool",
    );
}

#[tokio::test]
async fn replaying_a_serialized_live_journal_rebuilds_the_same_tool_message() {
    let expected_blocks = runtime_provider_blocks();
    let live_journal = Arc::new(InMemoryJournalStorage::new());
    let mut live_tools = ToolRegistry::new();
    live_tools
        .register(Box::new(EchoTool))
        .expect("echo tool registration should succeed");
    let mut live_agent = build_loop(
        FakeProvider::new(vec![tool_call_response(
            "echo",
            typed_echo_arguments("recorded image", &expected_blocks),
        )]),
        live_tools,
        Box::new(PassthroughContext),
        live_journal.clone(),
        default_budget(),
    );
    let mut live_messages = conversation("replay tool blocks");
    live_agent
        .run_single_turn(&mut live_messages)
        .await
        .expect("live tool turn should succeed");
    let live_message = tool_message(&live_messages, "tc-1");
    assert_eq!(live_message.content, "recorded image");
    assert_eq!(live_message.provider_content, expected_blocks);

    // Persist the live journal the way a real store would: through serde.
    let live_entries = live_journal
        .read_all(&AgentId("test-agent".into()))
        .expect("live journal should be readable");
    let persisted = serde_json::to_string(&live_entries).expect("journal entries should serialize");
    let replay_entries: Vec<JournalEntry> =
        serde_json::from_str(&persisted).expect("persisted journal entries should deserialize");
    assert_eq!(replay_entries.len(), live_entries.len());

    // The provider has no responses and no tool is registered: anything the
    // replayed turn does not restore from the journal fails or comes back as
    // an unknown-tool error, never as a re-execution that happens to agree.
    let mut replay_agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(FakeProvider::new(vec![])),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        Box::new(FixedClock(1000)),
        Some(replay_entries),
    );
    let mut replayed_messages = conversation("replay tool blocks");

    let result = replay_agent
        .run_single_turn(&mut replayed_messages)
        .await
        .expect("replay turn should succeed");

    assert!(matches!(result, TurnResult::ToolCallsProcessed { .. }));
    let replayed_message = tool_message(&replayed_messages, "tc-1");
    assert_eq!(replayed_message.content, live_message.content);
    assert_eq!(
        replayed_message.provider_content,
        live_message.provider_content
    );
    assert_eq!(replayed_message.provider_content, expected_blocks);
}

#[tokio::test]
async fn legacy_tool_result_entry_without_provider_content_replays_with_empty_blocks() {
    let mut replay_entries = vec![
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
                token_usage: TokenUsage::default(),
                finish_reason: "ToolUse".into(),
                assistant_message: Some(
                    multi_tool_call_response(vec![ToolCallMessage {
                        id: "legacy".into(),
                        name: "echo".into(),
                        arguments: serde_json::json!({"case": "legacy"}),
                    }])
                    .message,
                ),
            },
        ),
        replay_entry(
            4,
            JournalEntryKind::ToolCall {
                tool_call_id: Some("legacy".into()),
                tool_name: "echo".into(),
                arguments: serde_json::json!({"case": "legacy"}),
            },
        ),
    ];
    // A journal written before tool results carried provider blocks: the
    // `ToolResult` entry has no `provider_content` key at all.
    replay_entries.push(
        serde_json::from_value(serde_json::json!({
            "schema_version": JOURNAL_SCHEMA_VERSION,
            "agent_id": "test-agent",
            "timestamp_ms": 5,
            "entry": {
                "type": "ToolResult",
                "tool_call_id": "legacy",
                "tool_name": "echo",
                "content": "legacy recorded text",
                "is_error": false
            }
        }))
        .expect("a tool result entry without provider_content must still deserialize"),
    );

    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(FakeProvider::new(vec![])),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        Box::new(FixedClock(1000)),
        Some(replay_entries),
    );
    let mut messages = conversation("replay legacy tool result");

    let result = agent
        .run_single_turn(&mut messages)
        .await
        .expect("legacy replay turn should succeed");

    assert!(matches!(result, TurnResult::ToolCallsProcessed { .. }));
    let legacy = tool_message(&messages, "legacy");
    assert_eq!(legacy.content, "legacy recorded text");
    assert_eq!(
        legacy.provider_content,
        Vec::<simulacra_types::ProviderContentBlock>::new()
    );
}

#[tokio::test]
async fn a_failing_call_after_one_with_blocks_does_not_inherit_them() {
    let expected_blocks = runtime_provider_blocks();
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(EchoTool))
        .expect("echo tool registration should succeed");
    let mut agent = build_loop(
        FakeProvider::new(vec![multi_tool_call_response(vec![
            ToolCallMessage {
                id: "with-blocks".into(),
                name: "echo".into(),
                arguments: typed_echo_arguments("image attached", &expected_blocks),
            },
            ToolCallMessage {
                id: "no-blocks".into(),
                name: "missing_tool".into(),
                arguments: serde_json::json!({}),
            },
        ])]),
        tools,
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
    );
    let mut messages = conversation("one tool with blocks, one that fails");

    agent
        .run_single_turn(&mut messages)
        .await
        .expect("a failing second call should still produce both tool results");

    assert_eq!(
        tool_message(&messages, "with-blocks").provider_content,
        expected_blocks
    );
    let failed = tool_message(&messages, "no-blocks");
    assert_eq!(
        failed.content,
        "ERROR: execution failed: unknown tool: missing_tool"
    );
    assert_eq!(
        failed.provider_content,
        Vec::<simulacra_types::ProviderContentBlock>::new()
    );
}
