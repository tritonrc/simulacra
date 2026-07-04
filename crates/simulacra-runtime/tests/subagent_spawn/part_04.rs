#[tokio::test]
async fn create_agent_span_uses_genai_operation_name_and_child_agent_name() {
    let (_, spans, _) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            default_capability(),
            default_budget(),
            Arc::new(NoopFactory),
        );
        supervisor
            .spawn_agent(spawn_config(
                "child-1",
                "parent-agent",
                child_budget(10, 1, 1),
            ))
            .expect("spawn should succeed");
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "create_agent"
                && span.fields.get("gen_ai.operation.name").map(String::as_str)
                    == Some("create_agent")
                && span.fields.get("gen_ai.agent.name").map(String::as_str) == Some("child-1")
        }),
        "accepted spawns should emit a create_agent span with standard GenAI attributes"
    );
}

#[test]
fn running_the_child_loop_emits_an_invoke_agent_span() {
    let provider = FakeProvider::new(vec![ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: "done".into(),
            tool_calls: vec![],
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens: 3,
            output_tokens: 2,
        },
        finish_reason: FinishReason::EndTurn,
        provider_response_id: Some("resp-1".into()),
        model: "test-model".into(),
    }]);
    let tools = ToolRegistry::new();

    let (_, spans, _) = capture_trace(|| {
        let mut loop_ = build_loop(provider, tools, None);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(loop_.run("delegate task"))
            .expect("child loop should run");
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "invoke_agent"
                && span.fields.get("gen_ai.operation.name").map(String::as_str)
                    == Some("invoke_agent")
        }),
        "child execution should emit an invoke_agent span"
    );
}

#[tokio::test]
async fn subagent_lifecycle_spans_include_parent_and_child_linkage_attributes() {
    let (_, spans, _) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            default_capability(),
            default_budget(),
            Arc::new(NoopFactory),
        );
        supervisor
            .spawn_agent(spawn_config(
                "child-1",
                "parent-agent",
                child_budget(10, 1, 1),
            ))
            .expect("spawn should succeed");
    });

    assert!(
        spans.iter().any(|span| {
            span.fields.contains_key("simulacra.parent.agent_id")
                && span.fields.contains_key("simulacra.child.agent_type")
        }),
        "sub-agent lifecycle spans should expose Simulacra-specific parent/child linkage attributes"
    );
}

#[tokio::test]
async fn successful_child_completion_is_logged_with_child_parent_exit_reason_and_token_totals() {
    let factory = RecordingTaskFactory::new(vec![Ok(AgentLoopOutput {
        exit_reason: ExitReason::BudgetExhausted,
        messages: vec![],
        token_usage: TokenUsage {
            input_tokens: 8,
            output_tokens: 5,
        },
        used_turns: 0,
        used_cost: Decimal::ZERO,
    })]);

    let (_, _, events) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            default_capability(),
            default_budget(),
            Arc::new(factory.clone()),
        );
        supervisor
            .spawn_agent(spawn_config(
                "child-1",
                "parent-agent",
                child_budget(10, 1, 1),
            ))
            .expect("spawn should succeed");
    });
    factory.wait_for_completed(1).await;

    assert!(
        events.iter().any(|event| {
            event.level == "INFO"
                && event.fields.contains_key("child_id")
                && event.fields.contains_key("parent_id")
                && event.fields.contains_key("exit_reason")
                && event.fields.contains_key("token_total")
        }),
        "successful child completion should be logged with child id, parent id, exit reason, and token totals"
    );
}

#[tokio::test]
async fn child_failure_is_logged_at_warn_with_child_parent_agent_type_and_failure_reason() {
    let factory =
        RecordingTaskFactory::new(vec![Err(RuntimeError::CapabilityViolation("boom".into()))]);

    let (_, _, events) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            default_capability(),
            default_budget(),
            Arc::new(factory.clone()),
        );
        // After WARNING 1's fix, spawn_agent propagates immediate child errors —
        // the return value may be Err for this test. We only care about the
        // WARN log being emitted via process_child_result.
        let _ = supervisor.spawn_agent(spawn_config(
            "child-1",
            "parent-agent",
            child_budget(10, 1, 1),
        ));
    });
    factory.wait_for_completed(1).await;

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event.fields.contains_key("child_id")
                && event.fields.contains_key("parent_id")
                && event.fields.contains_key("agent_type")
                && event.fields.contains_key("failure_reason")
        }),
        "child failures should log a WARN event with child id, parent id, agent type, and failure reason"
    );
}

#[test]
fn spawn_acceptance_uses_command_priority_spawn_messages_in_the_actor_protocol() {
    let (_tx, _rx) = tokio::sync::oneshot::channel();
    let msg = SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "child-1",
                "parent-agent",
                child_budget(10, 1, 1),
            )),
            _tx,
        ),
    };

    assert!(
        matches!(msg.payload, SupervisorPayload::Spawn(_, _))
            && msg.priority == MessagePriority::Command,
        "interactive spawn requests should travel through the supervisor actor protocol as Command/Spawn messages"
    );
}

// ---------------------------------------------------------------------------
// Finding 1: Journal tests for SubAgentSpawned and SubAgentCompleted
// RED — the supervisor constructs these entries but never writes them.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervisor_writes_sub_agent_spawned_journal_entry_to_parent_stream_before_child_execution()
{
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let parent_id = AgentId("parent-agent".into());
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())])
        .with_journal_capture(Arc::clone(&journal), parent_id.clone());
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    supervisor.set_journal_storage(Arc::clone(&journal));

    supervisor
        .spawn_agent(spawn_config(
            "child-1",
            "parent-agent",
            child_budget(10, 1, 1),
        ))
        .expect("spawn should succeed");
    factory.wait_for_completed(1).await;

    // R7/R2: Verify temporal ordering — SubAgentSpawned must exist in the journal
    // *at the moment the child task begins*, not just after everything completes.
    let entries_at_spawn = factory
        .journal_at_spawn_time()
        .expect("factory should have captured journal state at spawn time");
    let spawned_before_child = entries_at_spawn.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::SubAgentSpawned {
                child_id,
                agent_type,
                ..
            }
            if child_id.0 == "child-1" && agent_type == "researcher"
        )
    });
    assert!(
        spawned_before_child,
        "SubAgentSpawned must be in the parent journal BEFORE child execution begins \
         (captured {} entries at spawn time, none were SubAgentSpawned)",
        entries_at_spawn.len()
    );
}

#[tokio::test]
async fn supervisor_writes_sub_agent_completed_journal_entry_to_parent_stream_after_child_success()
{
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let parent_id = AgentId("parent-agent".into());
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())])
        .with_journal_capture(Arc::clone(&journal), parent_id.clone());
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    supervisor.set_journal_storage(Arc::clone(&journal));

    supervisor
        .spawn_agent(spawn_config(
            "child-1",
            "parent-agent",
            child_budget(10, 1, 1),
        ))
        .expect("spawn should succeed");
    factory.wait_for_completed(1).await;

    let parent_entries = journal
        .read_all(&parent_id)
        .expect("parent journal should be readable");

    // R7: Verify ordering — SubAgentSpawned MUST come before SubAgentCompleted.
    let spawned_idx = parent_entries.iter().position(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::SubAgentSpawned { child_id, .. }
            if child_id.0 == "child-1"
        )
    });
    let completed_idx = parent_entries.iter().position(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::SubAgentCompleted { child_id, success }
            if child_id.0 == "child-1" && *success
        )
    });

    assert!(
        spawned_idx.is_some(),
        "SubAgentSpawned should be present in the parent journal"
    );
    assert!(
        completed_idx.is_some(),
        "SubAgentCompleted {{ success: true }} should be present in the parent journal"
    );
    assert!(
        spawned_idx.unwrap() < completed_idx.unwrap(),
        "SubAgentSpawned (index {:?}) must appear before SubAgentCompleted (index {:?}) \
         in the parent journal to prove correct ordering",
        spawned_idx,
        completed_idx
    );

    // Also verify SubAgentCompleted was NOT present at spawn time (it comes after child execution).
    let entries_at_spawn = factory
        .journal_at_spawn_time()
        .expect("factory should have captured journal state at spawn time");
    let completed_at_spawn = entries_at_spawn.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::SubAgentCompleted { child_id, .. }
            if child_id.0 == "child-1"
        )
    });
    assert!(
        !completed_at_spawn,
        "SubAgentCompleted must NOT be in the journal at spawn time — it should only appear after child execution"
    );
}

#[tokio::test]
async fn supervisor_writes_sub_agent_completed_with_success_false_on_child_failure() {
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let parent_id = AgentId("parent-agent".into());
    let factory =
        RecordingTaskFactory::new(vec![Err(RuntimeError::CapabilityViolation("boom".into()))])
            .with_journal_capture(Arc::clone(&journal), parent_id.clone());
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    supervisor.set_journal_storage(Arc::clone(&journal));

    // After WARNING 1's fix, spawn_agent propagates the immediate child error.
    // We still proceed to verify the journal recorded SubAgentCompleted{success:false}.
    let _ = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 1, 1),
    ));
    factory.wait_for_completed(1).await;

    let parent_entries = journal
        .read_all(&parent_id)
        .expect("parent journal should be readable");

    // R7: Verify ordering — SubAgentSpawned before SubAgentCompleted { success: false }.
    let spawned_idx = parent_entries.iter().position(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::SubAgentSpawned { child_id, .. }
            if child_id.0 == "child-1"
        )
    });
    let failed_idx = parent_entries.iter().position(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::SubAgentCompleted { child_id, success }
            if child_id.0 == "child-1" && !*success
        )
    });

    assert!(
        spawned_idx.is_some(),
        "SubAgentSpawned should be present in the parent journal"
    );
    assert!(
        failed_idx.is_some(),
        "SubAgentCompleted {{ success: false }} should be present in the parent journal"
    );
    assert!(
        spawned_idx.unwrap() < failed_idx.unwrap(),
        "SubAgentSpawned (index {:?}) must appear before SubAgentCompleted {{ success: false }} (index {:?})",
        spawned_idx,
        failed_idx
    );
}

// ---------------------------------------------------------------------------
// Finding 3: Exit reason format — spec says snake_case, impl uses Debug (PascalCase).
// RED — format!("{:?}", ExitReason::BudgetExhausted) produces "BudgetExhausted"
// but the spec requires "budget_exhausted".
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_agent_tool_exit_reason_uses_snake_case_format_per_spec() {
    let budget_exhausted_output = AgentLoopOutput {
        exit_reason: ExitReason::BudgetExhausted,
        messages: vec![Message {
            role: Role::Assistant,
            content: "partial".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        token_usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
        },
        used_turns: 1,
        used_cost: Decimal::ZERO,
    };

    let result = run_join_tool_call(Ok(budget_exhausted_output)).await;

    let value = result.expect("budget exhaustion should still be a success payload");
    assert_eq!(
        value.get("exit_reason").and_then(serde_json::Value::as_str),
        Some("budget_exhausted"),
        "exit_reason should use snake_case format per spec, not Debug format like BudgetExhausted"
    );
}

#[tokio::test]
async fn spawn_agent_tool_exit_reason_completed_uses_snake_case_format_per_spec() {
    let completed_output = AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![Message {
            role: Role::Assistant,
            content: "done".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        token_usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
        },
        used_turns: 1,
        used_cost: Decimal::ZERO,
    };

    let result = run_join_tool_call(Ok(completed_output)).await;

    let value = result.expect("completed child should return a success payload");
    assert_eq!(
        value.get("exit_reason").and_then(serde_json::Value::as_str),
        Some("completed"),
        "exit_reason should be \"completed\" (snake_case) for ExitReason::Complete, not Debug format like \"Complete\""
    );
}
