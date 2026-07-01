#[tokio::test]
async fn replay_fails_immediately_when_turn_start_entry_is_shifted() {
    struct PanickingProvider;

    impl Provider for PanickingProvider {
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
            panic!("Provider::chat should not be called after replay divergence");
        }
    }

    let journal = Arc::new(InMemoryJournalStorage::new());
    let replay_entries = vec![JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: AgentId("test-agent".into()),
        timestamp_ms: 1,
        entry: JournalEntryKind::LlmRequest {
            model: "test-model".into(),
            message_count: 2,
        },
    }];

    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(PanickingProvider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal,
        default_budget(),
        Box::new(FixedClock(2000)),
        Some(replay_entries),
    );

    let error = agent
        .run("replayed task")
        .await
        .expect_err("shifted replay should fail before provider call");
    let message = error.to_string();
    assert!(
        message.contains("replay divergence")
            && message.contains("TurnStart")
            && message.contains("LlmRequest"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn replay_fails_when_recorded_tool_call_does_not_match_live_tool_call() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let assistant_message = tool_call_response("echo", serde_json::json!({"msg": "live"})).message;
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
                model: "test-model".into(),
                token_usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
                finish_reason: "ToolUse".into(),
                assistant_message: Some(assistant_message),
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 4,
            entry: JournalEntryKind::ToolCall {
                tool_call_id: Some("tc-1".into()),
                tool_name: "echo".into(),
                arguments: serde_json::json!({"msg": "recorded"}),
            },
        },
    ];

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool));
    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(FakeProvider::new(vec![])),
        tools,
        Box::new(PassthroughContext),
        journal,
        default_budget(),
        Box::new(FixedClock(2000)),
        Some(replay_entries),
    );
    let mut messages = vec![
        Message {
            role: Role::System,
            content: "system".into(),
            tool_calls: vec![],
            tool_call_id: None,
        },
        Message {
            role: Role::User,
            content: "use echo".into(),
            tool_calls: vec![],
            tool_call_id: None,
        },
    ];

    let error = agent
        .run_single_turn(&mut messages)
        .await
        .expect_err("mismatched ToolCall arguments should fail replay");
    let message = error.to_string();
    assert!(
        message.contains("replay divergence")
            && message.contains("ToolCall")
            && message.contains("recorded"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn replay_tool_result_skips_nested_sandbox_entries_between_tool_call_and_final_result() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let assistant_message = tool_call_response("echo", serde_json::json!({"msg": "live"})).message;
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
                model: "test-model".into(),
                token_usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
                finish_reason: "ToolUse".into(),
                assistant_message: Some(assistant_message),
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 4,
            entry: JournalEntryKind::ToolCall {
                tool_call_id: Some("tc-1".into()),
                tool_name: "echo".into(),
                arguments: serde_json::json!({"msg": "live"}),
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 5,
            entry: JournalEntryKind::ShellCommand {
                command: "node /workspace/script.js".into(),
                exit_code: 0,
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 6,
            entry: JournalEntryKind::CodeExecution {
                language: "javascript".into(),
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 7,
            entry: JournalEntryKind::FileWrite {
                path: "/workspace/out.txt".into(),
                size_bytes: 5,
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 8,
            entry: JournalEntryKind::HttpRequest {
                method: "GET".into(),
                url: "https://example.test/".into(),
                status: 200,
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 9,
            entry: JournalEntryKind::ToolResult {
                tool_call_id: None,
                tool_name: "echo".into(),
                content: "nested collision".into(),
                is_error: false,
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 10,
            entry: JournalEntryKind::ToolResult {
                tool_call_id: Some("tc-1".into()),
                tool_name: "echo".into(),
                content: "recorded final".into(),
                is_error: false,
            },
        },
    ];

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool));
    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(FakeProvider::new(vec![])),
        tools,
        Box::new(PassthroughContext),
        journal,
        default_budget(),
        Box::new(FixedClock(2000)),
        Some(replay_entries),
    );
    let mut messages = vec![
        Message {
            role: Role::System,
            content: "system".into(),
            tool_calls: vec![],
            tool_call_id: None,
        },
        Message {
            role: Role::User,
            content: "use echo".into(),
            tool_calls: vec![],
            tool_call_id: None,
        },
    ];

    let result = agent
        .run_single_turn(&mut messages)
        .await
        .expect("replay should skip nested sandbox entries");

    match result {
        TurnResult::ToolCallsProcessed { tool_results, .. } => {
            assert_eq!(tool_results.len(), 1);
            assert_eq!(tool_results[0].content, "recorded final");
        }
        other => panic!("expected replayed tool call processing, got {other:?}"),
    }
}

#[tokio::test]
async fn replay_fails_when_current_tool_result_id_is_missing_after_nested_collision() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let assistant_message = tool_call_response("echo", serde_json::json!({"msg": "live"})).message;
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
                model: "test-model".into(),
                token_usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
                finish_reason: "ToolUse".into(),
                assistant_message: Some(assistant_message),
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 4,
            entry: JournalEntryKind::ToolCall {
                tool_call_id: Some("tc-1".into()),
                tool_name: "echo".into(),
                arguments: serde_json::json!({"msg": "live"}),
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 5,
            entry: JournalEntryKind::ToolResult {
                tool_call_id: None,
                tool_name: "echo".into(),
                content: "nested same-name result".into(),
                is_error: false,
            },
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 6,
            entry: JournalEntryKind::LlmRequest {
                model: "test-model".into(),
                message_count: 4,
            },
        },
    ];

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool));
    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(FakeProvider::new(vec![])),
        tools,
        Box::new(PassthroughContext),
        journal,
        default_budget(),
        Box::new(FixedClock(2000)),
        Some(replay_entries),
    );
    let mut messages = vec![
        Message {
            role: Role::System,
            content: "system".into(),
            tool_calls: vec![],
            tool_call_id: None,
        },
        Message {
            role: Role::User,
            content: "use echo".into(),
            tool_calls: vec![],
            tool_call_id: None,
        },
    ];

    let error = agent
        .run_single_turn(&mut messages)
        .await
        .expect_err("replay must not use a nested same-name result without the current id");
    let message = error.to_string();
    assert!(
        message.contains("expected ToolResult for echo (tc-1)"),
        "unexpected error: {message}"
    );
}

