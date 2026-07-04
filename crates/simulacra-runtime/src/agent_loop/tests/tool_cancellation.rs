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
