#[tokio::test]
async fn parent_max_sub_agents_zero_means_unlimited_sub_agents_not_already_exhausted() {
    let mut parent_budget = default_budget();
    parent_budget.max_sub_agents = 0;
    parent_budget.used_sub_agents = 0;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(NoopFactory),
    );

    let spawn = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 2, 1),
    ));

    assert!(
        spawn.is_ok(),
        "max_sub_agents = 0 should mean unlimited for the parent reservation check"
    );
}

#[tokio::test]
async fn parent_max_tokens_zero_means_unlimited_tokens_for_child_budget_requests() {
    let mut parent_budget = default_budget();
    parent_budget.max_tokens = 0;
    parent_budget.used_tokens = 91;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(NoopFactory),
    );

    let spawn = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(50, 2, 1),
    ));

    assert!(
        spawn.is_ok(),
        "max_tokens = 0 should mean unlimited, even when used_tokens is already non-zero"
    );
}

#[tokio::test]
async fn parent_max_turns_zero_means_unlimited_turns_not_already_exhausted() {
    let mut parent_budget = default_budget();
    parent_budget.max_turns = 0;
    parent_budget.used_turns = 9;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(NoopFactory),
    );

    let spawn = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 50, 1),
    ));

    assert!(
        spawn.is_ok(),
        "max_turns = 0 should mean unlimited for parent turn reservations"
    );
}

#[tokio::test]
async fn parent_max_cost_zero_means_unlimited_cost_not_already_exhausted() {
    let mut parent_budget = default_budget();
    parent_budget.max_cost = Decimal::ZERO;
    parent_budget.used_cost = Decimal::new(999, 2);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(NoopFactory),
    );

    let spawn = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget_with_cost(10, 1, Decimal::new(500, 2), 1),
    ));

    assert!(
        spawn.is_ok(),
        "max_cost = 0 should mean unlimited for parent cost reservations"
    );
}

#[test]
fn child_budget_request_exceeding_parent_remaining_budget_is_rejected_before_child_execution() {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.used_tokens = 95;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(factory.clone()),
    );

    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 1, 1),
    ));

    assert!(
        matches!(result, Err(RuntimeError::BudgetExhausted(_))),
        "budget reservations that exceed remaining headroom must fail before child execution"
    );
    assert_eq!(
        factory.started_count(),
        0,
        "no child task should start when the reservation is rejected"
    );
}

#[tokio::test]
async fn child_turn_budget_request_exceeding_parent_remaining_turns_is_rejected_before_child_execution()
 {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.used_turns = 9;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(factory.clone()),
    );

    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 2, 1),
    ));

    assert!(
        matches!(result, Err(RuntimeError::BudgetExhausted(_))),
        "child max_turns should be checked against the parent's remaining turns before execution"
    );
    assert_eq!(
        factory.started_count(),
        0,
        "no child task should start when the turn reservation exceeds the parent's remaining turns"
    );
}

#[tokio::test]
async fn child_cost_budget_request_exceeding_parent_remaining_cost_is_rejected_before_child_execution()
 {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.used_cost = Decimal::new(9950, 2);
    parent_budget.max_cost = Decimal::new(10000, 2);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(factory.clone()),
    );

    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget_with_cost(10, 1, Decimal::new(100, 2), 1),
    ));

    assert!(
        matches!(result, Err(RuntimeError::BudgetExhausted(_))),
        "child max_cost should be checked against the parent's remaining cost before execution"
    );
    assert_eq!(
        factory.started_count(),
        0,
        "no child task should start when the cost reservation exceeds the parent's remaining budget"
    );
}

#[tokio::test]
async fn accepting_child_spawn_increments_parent_used_sub_agents() {
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
        .expect("spawn should succeed for a within-budget child");

    assert_eq!(supervisor.parent_budget().used_sub_agents, 1);
}

#[tokio::test]
async fn child_token_usage_is_rolled_up_from_agent_loop_output_not_stale_spawn_budget_clone() {
    let factory = RecordingTaskFactory::new(vec![Ok(AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![Message {
            role: Role::Assistant,
            content: "child result".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        token_usage: TokenUsage {
            input_tokens: 19,
            output_tokens: 23,
        },
        used_turns: 0,
        used_cost: Decimal::ZERO,
    })]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    supervisor
        .spawn_agent(spawn_config(
            "child-1",
            "parent-agent",
            child_budget(40, 2, 1),
        ))
        .expect("spawn should succeed");
    factory.wait_for_completed(1).await;

    assert_eq!(
        supervisor.parent_budget().used_tokens,
        42,
        "budget rollup should use AgentLoopOutput.token_usage totals, not a stale SpawnConfig clone"
    );
}

#[tokio::test]
async fn child_turn_and_cost_usage_are_rolled_up_from_agent_loop_output_not_stale_spawn_budget_clone()
 {
    let factory = RecordingTaskFactory::new(vec![Ok(AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![],
        token_usage: TokenUsage::default(),
        used_turns: 2,
        used_cost: Decimal::new(375, 2),
    })]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    supervisor
        .spawn_agent(spawn_config(
            "child-1",
            "parent-agent",
            child_budget(40, 2, 1),
        ))
        .expect("spawn should succeed");
    factory.wait_for_completed(1).await;

    let budget = supervisor.parent_budget();
    assert_eq!(
        budget.used_turns, 2,
        "budget rollup should use AgentLoopOutput.used_turns from the completed child"
    );
    assert_eq!(
        budget.used_cost,
        Decimal::new(375, 2),
        "budget rollup should use AgentLoopOutput.used_cost from the completed child"
    );
}

#[tokio::test]
async fn spawn_config_passes_agent_type_and_task_to_the_task_factory() {
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    supervisor
        .spawn_agent(spawn_config_with_agent_type(
            "child-1",
            "parent-agent",
            "reviewer",
            child_budget(10, 1, 1),
        ))
        .expect("spawn should succeed");
    factory.wait_for_completed(1).await;

    let started = factory.inner.started.lock().unwrap().clone();
    let snapshot = started
        .first()
        .expect("task factory should capture the spawn config");
    assert_eq!(snapshot.agent_type, "reviewer");
    assert_eq!(snapshot.task, "delegate task");
}

#[tokio::test]
async fn parent_receives_exactly_one_tool_result_message_per_spawn_agent_call() {
    // Exercise the real SpawnAgentTool path (not a fake) to verify the parent
    // receives exactly one result per spawn_agent call.
    let (result, _captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 1,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Ok(child_success_output()),
    )
    .await;

    // run_spawn_tool_call returns exactly one result (the return value of Tool::call).
    // The real SpawnAgentTool produces a single JSON payload, verifying the one-result contract.
    let value = result.expect("successful spawn should return a tool result");
    assert!(
        value.get("child_id").is_some(),
        "the single tool result should contain child_id"
    );
    assert!(
        value.get("exit_reason").is_some(),
        "the single tool result should contain exit_reason"
    );
}

#[tokio::test]
async fn failed_spawn_agent_calls_return_error_tool_results_with_child_id_agent_type_and_error() {
    // Exercise the real SpawnAgentTool path with a child runtime failure.
    let (result, _captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 1,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Err(RuntimeError::CapabilityViolation("shell denied".into())),
    )
    .await;

    match result {
        Err(ToolError::ExecutionFailed(msg)) => {
            assert!(
                msg.contains("child_id") || msg.contains("child-"),
                "error message should reference the child_id: {msg}"
            );
            assert!(
                msg.contains("researcher"),
                "error message should reference the agent_type: {msg}"
            );
            assert!(
                msg.contains("shell denied") || msg.contains("failed"),
                "error message should contain the failure reason: {msg}"
            );
        }
        other => panic!(
            "failed spawn_agent should return Err(ToolError::ExecutionFailed), got {other:?}"
        ),
    }
}

#[tokio::test]
async fn spawn_agent_tool_parses_capabilities_override_json_into_spawn_config() {
    let (result, captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 1,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            },
            "capabilities": {
                "network": ["net:api.github.com"],
                "mcp_tools": ["github"],
                "shell": true,
                "javascript": true,
                "python": false,
                "paths_write": ["/workspace/out.txt"],
                "paths_read": ["/workspace/in.txt"],
                "spawn_types": ["reviewer"]
            }
        }),
        &["researcher"],
        Ok(child_success_output()),
    )
    .await;

    result.expect("successful child result should still return a tool payload");
    let cap = captured
        .capability
        .as_ref()
        .expect("capability should be Some when LLM provides capabilities");
    assert_eq!(
        cap.network,
        vec![NetworkPermission("net:api.github.com".into())]
    );
    assert_eq!(cap.mcp_tools, vec!["github".to_string()]);
    assert!(cap.shell);
    assert!(cap.javascript);
    assert_eq!(
        cap.paths_write,
        vec![PathPattern("/workspace/out.txt".into())]
    );
    assert_eq!(
        cap.paths_read,
        vec![PathPattern("/workspace/in.txt".into())]
    );
    assert_eq!(cap.spawn_types, vec!["reviewer".to_string()]);
}

#[tokio::test]
async fn spawn_agent_tool_child_runtime_failures_return_toolerror_execution_failed() {
    let (result, _captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 1,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Err(RuntimeError::CapabilityViolation("shell denied".into())),
    )
    .await;

    assert!(
        matches!(result, Err(ToolError::ExecutionFailed(_))),
        "child runtime failures should surface as Err(ToolError::ExecutionFailed(...)) so AgentLoop marks the tool result as is_error"
    );
}

#[tokio::test]
async fn spawn_agent_tool_does_not_hardcode_parent_agent_id_in_spawn_config() {
    let (_result, captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 1,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Ok(child_success_output()),
    )
    .await;

    assert_eq!(
        captured.parent_id,
        AgentId("parent-agent".into()),
        "SpawnAgentTool should propagate the caller's parent AgentId into SpawnConfig"
    );
}

