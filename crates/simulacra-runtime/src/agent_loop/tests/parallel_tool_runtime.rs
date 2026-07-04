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
