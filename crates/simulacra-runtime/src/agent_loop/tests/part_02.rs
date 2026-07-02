#[tokio::test]
async fn exhausted_budget_returns_error_without_calling_provider() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    // Provider that would panic if called
    let provider = FakeProvider::new(vec![]);

    // Budget already exhausted: used_turns == max_turns
    let mut budget = ResourceBudget::new(100_000, 1, Decimal::new(100, 0), 5);
    budget.used_turns = 1;

    let mut agent = build_loop(
        provider,
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal.clone(),
        budget,
    );

    let result = agent.run("This should fail").await;
    assert!(result.is_err());

    // Provider should not have been called — no journal entries for LlmRequest
    let entries = journal
        .read_all(&AgentId("test-agent".into()))
        .expect("read_all should succeed");
    assert!(
        entries
            .iter()
            .all(|e| !matches!(e.entry, JournalEntryKind::LlmRequest { .. })),
        "provider should not have been called"
    );
}

// -----------------------------------------------------------------------
// Test 6: Context compaction
// -----------------------------------------------------------------------
#[tokio::test]
async fn context_strategy_compacts_messages() {
    let journal = Arc::new(InMemoryJournalStorage::new());

    // Use a truncating context strategy that keeps only system + last 1 message
    let context = TruncatingContext { keep_recent: 1 };

    // Two turns: tool call then text. The second call should receive compacted messages.
    let provider = FakeProvider::new(vec![
        tool_call_response("echo", serde_json::json!({"n": 1})),
        text_response("Final"),
    ]);
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(EchoTool))
        .expect("test tool registration should succeed");

    let mut agent = build_loop(
        provider,
        tools,
        Box::new(context),
        journal.clone(),
        default_budget(),
    );

    let output = agent
        .run("Use echo then finish")
        .await
        .expect("run should succeed");

    // The loop should complete successfully even with aggressive compaction
    assert_eq!(output.exit_reason, ExitReason::Complete);
    // Full message history preserved in output (compaction only affects provider input)
    assert_eq!(output.messages.len(), 5);
}

#[test]
fn compaction_window_is_bounded_by_context_not_just_budget() {
    let capped = compaction_token_limit(10_000_000, 0);
    assert_eq!(capped, CONTEXT_TOKEN_LIMIT);

    assert_eq!(compaction_token_limit(64_000, 59_000), 5_000);

    let unlimited = compaction_token_limit(0, 0);
    assert_eq!(unlimited, CONTEXT_TOKEN_LIMIT);
}

// -----------------------------------------------------------------------
// Test 7: Token usage accumulates across turns
// -----------------------------------------------------------------------
#[tokio::test]
async fn token_usage_accumulates_across_turns() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![
        tool_call_response("echo", serde_json::json!({})),
        text_response("done"),
    ]);
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(EchoTool))
        .expect("test tool registration should succeed");

    let mut agent = build_loop(
        provider,
        tools,
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
    );

    let output = agent.run("go").await.expect("run should succeed");

    // Turn 1: 20 in + 10 out; Turn 2: 10 in + 5 out
    assert_eq!(output.token_usage.input_tokens, 30);
    assert_eq!(output.token_usage.output_tokens, 15);
    assert_eq!(output.token_usage.total(), 45);
}

// -----------------------------------------------------------------------
// Test 8: Budget tracks used_turns
// -----------------------------------------------------------------------
#[tokio::test]
async fn budget_used_turns_increments() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![
        tool_call_response("echo", serde_json::json!({})),
        text_response("done"),
    ]);
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(EchoTool))
        .expect("test tool registration should succeed");

    let budget = default_budget();
    let mut agent = build_loop(
        provider,
        tools,
        Box::new(PassthroughContext),
        journal.clone(),
        budget,
    );

    let _ = agent.run("go").await.expect("run should succeed");

    // The budget is internal to agent, so we verify via journal: 2 TurnStart entries
    let entries = journal
        .read_all(&AgentId("test-agent".into()))
        .expect("read_all should succeed");
    let turn_starts = entries
        .iter()
        .filter(|e| matches!(e.entry, JournalEntryKind::TurnStart))
        .count();
    assert_eq!(turn_starts, 2);
}

#[tokio::test]
async fn capability_denial_is_journaled_with_operation_details() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![
        tool_call_response("deny_shell", serde_json::json!({})),
        text_response("done"),
    ]);
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(DenyShellTool))
        .expect("test tool registration should succeed");

    let mut agent = build_loop(
        provider,
        tools,
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
    );

    let output = agent.run("try a denied tool").await.unwrap();

    let denied_result = journal
        .read_all(&AgentId("test-agent".into()))
        .unwrap()
        .into_iter()
        .find_map(|entry| match entry.entry {
            JournalEntryKind::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } if tool_name == "deny_shell" => Some((content, is_error)),
            _ => None,
        })
        .expect("expected a journaled tool result for the denied capability");

    assert_eq!(output.exit_reason, ExitReason::Complete);
    assert!(
        denied_result.1,
        "capability denial must be journaled as an error"
    );
    assert!(
        denied_result.0.contains("shell"),
        "journaled denial should include the denied operation"
    );
    assert!(
        denied_result.0.contains("shell capability not granted"),
        "journaled denial should include the denial reason"
    );
}

#[tokio::test]
async fn explicit_tool_output_is_error_true_is_journaled_and_prefixed() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![
        tool_call_response("explicit_error_output", serde_json::json!({})),
        text_response("done"),
    ]);
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(ExplicitErrorOutputTool))
        .expect("test tool registration should succeed");

    let mut agent = build_loop(
        provider,
        tools,
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
    );

    let output = agent.run("run explicit error tool").await.unwrap();

    let journaled = journal
        .read_all(&AgentId("test-agent".into()))
        .unwrap()
        .into_iter()
        .find_map(|entry| match entry.entry {
            JournalEntryKind::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } if tool_name == "explicit_error_output" => Some((content, is_error)),
            _ => None,
        })
        .expect("expected explicit error tool result");

    assert_eq!(journaled, ("explicit failure".to_string(), true));
    assert!(output.messages.iter().any(|message| {
        message.role == Role::Tool && message.content == "ERROR: explicit failure"
    }));
}

#[tokio::test]
async fn builtin_tool_output_error_is_journaled_and_prefixed() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![
        tool_call_response("file_read", serde_json::json!({ "path": "/missing.txt" })),
        text_response("done"),
    ]);
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(NamedErrorOutputTool {
            name: "file_read",
            content: "not found: /missing.txt",
        }))
        .expect("test tool registration should succeed");

    let mut agent = build_loop(
        provider,
        tools,
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
    );

    let output = agent.run("read a missing file").await.unwrap();

    let journaled = journal
        .read_all(&AgentId("test-agent".into()))
        .unwrap()
        .into_iter()
        .find_map(|entry| match entry.entry {
            JournalEntryKind::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } if tool_name == "file_read" => Some((content, is_error)),
            _ => None,
        })
        .expect("expected file_read tool result");

    assert!(journaled.0.contains("not found"), "got {:?}", journaled.0);
    assert!(journaled.1);
    assert!(output.messages.iter().any(|message| {
        message.role == Role::Tool && message.content.starts_with("ERROR: ")
    }));
}

#[tokio::test]
async fn skill_tool_output_error_is_journaled_and_prefixed() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![
        tool_call_response("Skill", serde_json::json!({ "command": "missing" })),
        text_response("done"),
    ]);
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(NamedErrorOutputTool {
            name: "Skill",
            content: "unknown skill: \"missing\"",
        }))
        .expect("test tool registration should succeed");

    let mut agent = build_loop(
        provider,
        tools,
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
    );

    let output = agent.run("load a missing skill").await.unwrap();

    let journaled = journal
        .read_all(&AgentId("test-agent".into()))
        .unwrap()
        .into_iter()
        .find_map(|entry| match entry.entry {
            JournalEntryKind::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } if tool_name == "Skill" => Some((content, is_error)),
            _ => None,
        })
        .expect("expected Skill tool result");

    assert!(journaled.0.contains("unknown skill"), "got {:?}", journaled.0);
    assert!(journaled.1);
    assert!(output.messages.iter().any(|message| {
        message.role == Role::Tool && message.content.starts_with("ERROR: unknown skill")
    }));
}

#[tokio::test]
async fn legacy_error_field_without_explicit_is_error_is_not_journaled_as_error() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![
        tool_call_response("legacy_error_field", serde_json::json!({})),
        text_response("done"),
    ]);
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(LegacyErrorFieldTool))
        .expect("test tool registration should succeed");

    let mut agent = build_loop(
        provider,
        tools,
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
    );

    let output = agent.run("run legacy error-shaped tool").await.unwrap();

    let journaled = journal
        .read_all(&AgentId("test-agent".into()))
        .unwrap()
        .into_iter()
        .find_map(|entry| match entry.entry {
            JournalEntryKind::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } if tool_name == "legacy_error_field" => Some((content, is_error)),
            _ => None,
        })
        .expect("expected legacy error-field tool result");

    assert_eq!(journaled.0, r#"{"error":"legacy failure"}"#);
    assert!(!journaled.1, "plain error fields must not imply is_error");
    assert!(output.messages.iter().any(|message| {
        message.role == Role::Tool && message.content == r#"{"error":"legacy failure"}"#
    }));
}

#[tokio::test]
async fn memory_read_chunk_error_payload_is_journaled_as_tool_error() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![
        tool_call_response("memory_read_chunk", serde_json::json!({ "hit_id": "missing" })),
        text_response("done"),
    ]);

    let memory_scope = MemoryPath::parse("/var/memory/self").unwrap();
    let memory_capability = MemoryCapability {
        enabled: true,
        search_scopes: vec![memory_scope],
        write_scopes: Vec::new(),
    };
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(simulacra_tool::MemoryReadChunkTool::new(
            TenantId::parse("tenant-a").unwrap(),
            memory_capability,
            Arc::new(NoopMemoryStore),
            Arc::new(NoopVectorIndex),
            Arc::new(simulacra_memory::HitIdCache::new()),
            None,
        )))
        .expect("test tool registration should succeed");

    let mut agent = build_loop(
        provider,
        tools,
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
    );

    let output = agent.run("read a missing memory hit").await.unwrap();

    let journaled = journal
        .read_all(&AgentId("test-agent".into()))
        .unwrap()
        .into_iter()
        .find_map(|entry| match entry.entry {
            JournalEntryKind::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } if tool_name == "memory_read_chunk" => Some((content, is_error)),
            _ => None,
        })
        .expect("expected memory_read_chunk tool result");

    assert!(journaled.0.contains("hit_not_found"));
    assert!(journaled.1, "memory error payloads must set is_error");
    assert!(output.messages.iter().any(|message| {
        message.role == Role::Tool
            && message.content.starts_with("ERROR: ")
            && message.content.contains("hit_not_found")
    }));
}

// -----------------------------------------------------------------------
// S005: Injectable clock produces deterministic timestamps
// -----------------------------------------------------------------------

struct FixedClock(u64);

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

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
    // All entries should have the fixed timestamp
    for entry in &entries {
        assert_eq!(
            entry.timestamp_ms, 42_000,
            "all journal entries should use the injected clock"
        );
    }
}

// -----------------------------------------------------------------------
// S005: Replay with recorded LLM response does not make a real API call
// -----------------------------------------------------------------------
#[tokio::test]
async fn replay_with_recorded_llm_response_skips_provider() {
    let journal = Arc::new(InMemoryJournalStorage::new());

    // A provider that panics if called — proves replay skips it
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

    // Build a replay journal that represents one complete turn:
    // TurnStart, LlmRequest, LlmResponse (with EndTurn and no tool calls)
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

    // This should succeed without calling the provider
    let output = agent
        .run("replayed task")
        .await
        .expect("replay should succeed");

    assert_eq!(output.exit_reason, ExitReason::Complete);
    assert_eq!(output.token_usage.input_tokens, 10);
    assert_eq!(output.token_usage.output_tokens, 5);
}
