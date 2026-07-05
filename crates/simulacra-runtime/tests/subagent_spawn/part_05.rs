#[tokio::test]
async fn spawn_agent_tool_exit_reason_max_turns_uses_snake_case_format_per_spec() {
    let max_turns_output = AgentLoopOutput {
        exit_reason: ExitReason::MaxTurns,
        messages: vec![Message {
            role: Role::Assistant,
            content: "ran out of turns".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        token_usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
        },
        used_turns: 3,
        used_cost: Decimal::ZERO,
    };

    let result = run_join_tool_call(Ok(max_turns_output)).await;

    let value = result
        .expect("max_turns child should return a success payload (partial success, not error)");
    assert_eq!(
        value.get("exit_reason").and_then(serde_json::Value::as_str),
        Some("max_turns"),
        "exit_reason should be \"max_turns\" (snake_case) for ExitReason::MaxTurns, not Debug format like \"MaxTurns\""
    );
}

// ---------------------------------------------------------------------------
// Finding 4: Three-way capability intersection (parent, config, override).
// The existing test only checks two-way (config vs override). This adds a test
// where the parent, config, AND override all differ, asserting the intersection.
// ---------------------------------------------------------------------------

#[test]
fn agent_task_factory_performs_three_way_capability_intersection_parent_config_and_override() {
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-1",
        "model": "child-model",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "shell_exec",
                        "arguments": "{\"command\":\"echo hello\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    // Config grants: shell=true, javascript=true, python=false
    let child_config_capabilities = CapabilitiesConfig {
        network: vec![],
        mcp: vec![],
        shell: true,
        javascript: true,
        python: false,
        paths_read: vec!["/workspace/**".into()],
        paths_write: vec!["/workspace/**".into()],

        skill_patterns: vec![],

        memory: None,
    };

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let factory = AgentTaskFactory {
        config: task_factory_config(child_config_capabilities),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    // Override grants: shell=true, javascript=false
    // Parent grants: shell=false (via default CapabilityToken)
    // Intersection: shell should be false (parent denies), javascript should be false (override denies)
    let mut spawn = spawn_config("child-1", "parent-agent", child_budget(32, 1, 0));
    spawn.capability = Some(CapabilityToken {
        shell: true,
        javascript: false,
        ..Default::default()
    });

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("child task should complete");

    let tool_message = output
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("child loop should append the tool result");
    assert!(
        tool_message.content.starts_with("ERROR: ")
            && tool_message
                .content
                .contains("shell capability not granted"),
        "effective child capability should be the three-way intersection of parent token, \
         child type config, and the override — parent denies shell even though config and override allow it"
    );
}

// ---------------------------------------------------------------------------
// Finding 5: Exact-boundary budget tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_budget_exactly_equals_parent_remaining_budget_is_accepted() {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.max_tokens = 100;
    parent_budget.used_tokens = 90;
    let mut supervisor =
        AgentSupervisor::with_task_factory(default_capability(), parent_budget, Arc::new(factory));

    // Request exactly 10 tokens when parent has exactly 10 remaining
    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 1, 1),
    ));

    assert!(
        result.is_ok(),
        "child budget request that exactly equals the parent's remaining budget should be accepted"
    );
}

#[tokio::test]
async fn child_budget_one_token_over_parent_remaining_is_rejected() {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.max_tokens = 100;
    parent_budget.used_tokens = 90;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(factory.clone()),
    );

    // Request 11 tokens when parent has exactly 10 remaining
    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(11, 1, 1),
    ));

    assert!(
        matches!(result, Err(RuntimeError::BudgetExhausted(_))),
        "child budget request one token over parent's remaining budget should be rejected"
    );
    assert_eq!(factory.started_count(), 0);
}

#[tokio::test]
async fn child_turns_exactly_equals_parent_remaining_turns_is_accepted() {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.max_turns = 10;
    parent_budget.used_turns = 8;
    let mut supervisor =
        AgentSupervisor::with_task_factory(default_capability(), parent_budget, Arc::new(factory));

    // Request exactly 2 turns when parent has exactly 2 remaining
    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 2, 1),
    ));

    assert!(
        result.is_ok(),
        "child turn request that exactly equals the parent's remaining turns should be accepted"
    );
}

#[tokio::test]
async fn child_turns_one_over_parent_remaining_is_rejected() {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.max_turns = 10;
    parent_budget.used_turns = 8;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(factory.clone()),
    );

    // Request 3 turns when parent has exactly 2 remaining
    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 3, 1),
    ));

    assert!(
        matches!(result, Err(RuntimeError::BudgetExhausted(_))),
        "child turn request one over parent's remaining turns should be rejected"
    );
    assert_eq!(factory.started_count(), 0);
}

// ---------------------------------------------------------------------------
// Finding 6: Empty message field — run_spawn_tool_call with no assistant message.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_agent_tool_returns_empty_message_when_child_output_has_no_assistant_message() {
    let output_with_no_assistant = AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![
            // Only system and user messages, no assistant message
            Message {
                role: Role::System,
                content: "system prompt".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: "task".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ],
        token_usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 0,
        },
        used_turns: 0,
        used_cost: Decimal::ZERO,
    };

    let result = run_join_tool_call(Ok(output_with_no_assistant)).await;

    let value =
        result.expect("child with no assistant message should still return a success payload");
    assert_eq!(
        value.get("message").and_then(serde_json::Value::as_str),
        Some(""),
        "join_child_agent should return empty string for message when the child has no final assistant message"
    );
}

#[tokio::test]
async fn spawn_agent_tool_returns_empty_message_when_child_output_messages_list_is_empty() {
    let output_with_empty_messages = AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![],
        token_usage: TokenUsage::default(),
        used_turns: 0,
        used_cost: Decimal::ZERO,
    };

    let result = run_join_tool_call(Ok(output_with_empty_messages)).await;

    let value = result.expect("child with empty messages should still return a success payload");
    assert_eq!(
        value.get("message").and_then(serde_json::Value::as_str),
        Some(""),
        "join_child_agent should return empty string for message when the child messages list is empty"
    );
}

// ---------------------------------------------------------------------------
// Finding 7: Cancellation path — oneshot sender dropped.
// RED — SpawnAgentTool returns Ok(json!({..error..})) instead of Err(ToolError).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_agent_tool_returns_error_when_supervisor_drops_result_channel() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<SupervisorMessage>(1);
    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec!["researcher".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    };

    let call_future = tool.call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 10,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &CapabilityToken::default(),
    );

    let drop_future = async move {
        let message = receiver
            .recv()
            .await
            .expect("spawn tool should send one supervisor message");
        // Extract and immediately drop the oneshot sender to simulate cancellation
        match message.payload {
            SupervisorPayload::Spawn(_config, result_tx) => {
                drop(result_tx);
            }
            other => panic!("expected SupervisorPayload::Spawn, got {other:?}"),
        }
    };

    let (result, _) = tokio::join!(call_future, drop_future);

    assert!(
        matches!(result, Err(simulacra_types::ToolError::ExecutionFailed(_))),
        "spawn_agent should return Err(ToolError::ExecutionFailed) when the supervisor drops the result channel, \
         not Ok(json) with an error field"
    );
}

// ---------------------------------------------------------------------------
// Finding 2: Tests against real SpawnAgentTool definition shape.
// These test the actual SpawnAgentTool (not a fake) for definition correctness.
// ---------------------------------------------------------------------------

fn make_real_spawn_agent_tool() -> SpawnAgentTool {
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    SpawnAgentTool {
        sender,
        can_spawn: vec!["researcher".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    }
}

#[test]
fn real_spawn_agent_tool_definition_uses_the_documented_name_and_description() {
    let tool = make_real_spawn_agent_tool();
    let definition = tool.definition();

    assert_eq!(definition.name, "spawn_agent");
    assert_eq!(
        definition.description,
        "Spawn a supervised child agent to handle a delegated task and return a live child handle."
    );
}

#[test]
fn real_spawn_agent_tool_definition_exposes_agent_type_task_budget_and_capabilities() {
    let tool = make_real_spawn_agent_tool();
    let definition = tool.definition();
    let properties = definition
        .input_schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .expect("schema should expose properties");

    for field in ["agent_type", "task", "budget", "capabilities"] {
        assert!(
            properties.contains_key(field),
            "real spawn_agent schema should expose {field}"
        );
    }
}

#[test]
fn real_spawn_agent_tool_budget_schema_requires_all_fields_and_disallows_extras() {
    let tool = make_real_spawn_agent_tool();
    let definition = tool.definition();
    let budget = definition
        .input_schema
        .pointer("/properties/budget")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    assert_eq!(
        budget.get("required"),
        Some(&serde_json::json!([
            "max_tokens",
            "max_turns",
            "max_cost",
            "max_sub_agents"
        ]))
    );
    assert_eq!(
        budget.get("additionalProperties"),
        Some(&serde_json::Value::Bool(false))
    );
}

#[test]
fn real_spawn_agent_tool_capabilities_schema_matches_spec_shape() {
    let tool = make_real_spawn_agent_tool();
    let definition = tool.definition();
    let capabilities = definition
        .input_schema
        .pointer("/properties/capabilities/properties")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();

    for field in [
        "network",
        "mcp_tools",
        "shell",
        "javascript",
        "python",
        "paths_write",
        "paths_read",
        "spawn_types",
    ] {
        assert!(
            capabilities.contains_key(field),
            "real spawn_agent capability override schema should include {field}"
        );
    }
}

fn make_real_steer_child_agent_tool() -> SteerChildAgentTool {
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    SteerChildAgentTool { sender }
}

fn make_real_child_status_tool() -> ChildStatusTool {
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    ChildStatusTool { sender }
}

fn make_real_wait_child_agent_tool() -> WaitChildAgentTool {
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    WaitChildAgentTool { sender }
}

fn make_real_close_child_agent_tool() -> CloseChildAgentTool {
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    CloseChildAgentTool { sender }
}

#[test]
fn real_steer_child_agent_tool_definition_has_documented_schema() {
    let tool = make_real_steer_child_agent_tool();
    let definition = tool.definition();
    assert_eq!(definition.name, "steer_child_agent");
    assert_eq!(
        definition.input_schema["required"],
        serde_json::json!(["child_id", "message"])
    );
    assert_eq!(definition.input_schema["additionalProperties"], false);
    assert_eq!(
        definition.input_schema["properties"]["child_id"]["type"],
        "string"
    );
    assert_eq!(
        definition.input_schema["properties"]["message"]["type"],
        "string"
    );
}

#[tokio::test]
async fn steer_child_agent_tool_rejects_empty_child_id_and_blank_message() {
    let tool = make_real_steer_child_agent_tool();
    let empty_child = tool
        .call(
            serde_json::json!({ "child_id": "", "message": "keep going" }),
            &CapabilityToken::default(),
        )
        .await;
    assert!(matches!(
        empty_child,
        Err(ToolError::InvalidArguments(message)) if message.contains("child_id")
    ));

    let blank_message = tool
        .call(
            serde_json::json!({ "child_id": "child-1", "message": "   " }),
            &CapabilityToken::default(),
        )
        .await;
    assert!(matches!(
        blank_message,
        Err(ToolError::InvalidArguments(message)) if message.contains("message")
    ));
}

#[tokio::test]
async fn steer_child_agent_tool_sends_command_and_returns_queued_status() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let tool = SteerChildAgentTool { sender };
    let call = tool.call(
        serde_json::json!({ "child_id": "child-1", "message": "look at tests" }),
        &CapabilityToken::default(),
    );
    let receive = async move {
        let message = receiver
            .recv()
            .await
            .expect("steer tool should send one supervisor message");
        assert_eq!(message.priority, MessagePriority::Command);
        match message.payload {
            SupervisorPayload::SteerChild(child_id, steer_message, result_tx) => {
                assert_eq!(child_id.0, "child-1");
                assert_eq!(steer_message, "look at tests");
                result_tx
                    .send(Ok(()))
                    .expect("steer tool should await the queued ack");
            }
            other => panic!("expected SupervisorPayload::SteerChild, got {other:?}"),
        }
    };

    let (result, ()) = tokio::join!(call, receive);
    let result = result.expect("steer should return queued status");
    assert_eq!(
        result,
        serde_json::json!({ "child_id": "child-1", "status": "queued" })
    );
}

#[test]
fn s054_child_orchestration_tools_have_documented_schemas() {
    let status = make_real_child_status_tool().definition();
    assert_eq!(status.name, "child_status");
    assert_eq!(status.input_schema["required"], serde_json::json!(["child_id"]));
    assert_eq!(status.input_schema["additionalProperties"], false);
    assert_eq!(
        status.input_schema["properties"]["child_id"]["type"],
        "string"
    );

    let wait = make_real_wait_child_agent_tool().definition();
    assert_eq!(wait.name, "wait_child_agent");
    assert_eq!(
        wait.input_schema["required"],
        serde_json::json!(["child_id", "timeout_ms"])
    );
    assert_eq!(wait.input_schema["additionalProperties"], false);
    assert_eq!(
        wait.input_schema["properties"]["timeout_ms"]["minimum"],
        serde_json::json!(0)
    );

    let close = make_real_close_child_agent_tool().definition();
    assert_eq!(close.name, "close_child_agent");
    assert_eq!(close.input_schema["required"], serde_json::json!(["child_id"]));
    assert_eq!(close.input_schema["additionalProperties"], false);
}

#[tokio::test]
async fn s054_child_orchestration_tools_reject_invalid_arguments() {
    let status = make_real_child_status_tool()
        .call(serde_json::json!({ "child_id": "" }), &CapabilityToken::default())
        .await;
    assert!(matches!(
        status,
        Err(ToolError::InvalidArguments(message)) if message.contains("child_id")
    ));

    let missing_timeout = make_real_wait_child_agent_tool()
        .call(
            serde_json::json!({ "child_id": "child-1" }),
            &CapabilityToken::default(),
        )
        .await;
    assert!(matches!(
        missing_timeout,
        Err(ToolError::InvalidArguments(message)) if message.contains("timeout_ms")
    ));

    let negative_timeout = make_real_wait_child_agent_tool()
        .call(
            serde_json::json!({ "child_id": "child-1", "timeout_ms": -1 }),
            &CapabilityToken::default(),
        )
        .await;
    assert!(matches!(
        negative_timeout,
        Err(ToolError::InvalidArguments(message)) if message.contains("timeout_ms")
    ));

    let close = make_real_close_child_agent_tool()
        .call(serde_json::json!({}), &CapabilityToken::default())
        .await;
    assert!(matches!(
        close,
        Err(ToolError::InvalidArguments(message)) if message.contains("child_id")
    ));
}

#[tokio::test]
async fn child_status_tool_sends_command_and_returns_status_json() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let tool = ChildStatusTool { sender };
    let call = tool.call(
        serde_json::json!({ "child_id": "child-1" }),
        &CapabilityToken::default(),
    );
    let receive = async move {
        let message = receiver
            .recv()
            .await
            .expect("child_status should send one supervisor message");
        assert_eq!(message.priority, MessagePriority::Command);
        match message.payload {
            SupervisorPayload::ChildStatus(child_id, result_tx) => {
                assert_eq!(child_id.0, "child-1");
                result_tx
                    .send(Ok(simulacra_runtime::ChildStatus {
                        child_id,
                        agent_type: "researcher".into(),
                        status: "running".into(),
                        ready: false,
                        elapsed_ms: 12,
                    }))
                    .expect("child_status tool should await status response");
            }
            other => panic!("expected SupervisorPayload::ChildStatus, got {other:?}"),
        }
    };

    let (result, ()) = tokio::join!(call, receive);
    assert_eq!(
        result.expect("child_status should return status JSON"),
        serde_json::json!({
            "child_id": "child-1",
            "agent_type": "researcher",
            "status": "running",
            "ready": false,
            "elapsed_ms": 12
        })
    );
}

#[tokio::test]
async fn wait_child_agent_tool_sends_command_and_returns_running_or_terminal_json() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let tool = WaitChildAgentTool { sender };
    let call = tool.call(
        serde_json::json!({ "child_id": "child-1", "timeout_ms": 0 }),
        &CapabilityToken::default(),
    );
    let receive = async move {
        let message = receiver
            .recv()
            .await
            .expect("wait_child_agent should send one supervisor message");
        assert_eq!(message.priority, MessagePriority::Command);
        match message.payload {
            SupervisorPayload::WaitChild(child_id, timeout, result_tx) => {
                assert_eq!(child_id.0, "child-1");
                assert_eq!(timeout, Duration::ZERO);
                result_tx
                    .send(Ok(simulacra_runtime::WaitChildResult {
                        child_id,
                        agent_type: None,
                        status: "running".into(),
                        ready: false,
                        terminal: None,
                    }))
                    .expect("wait_child_agent tool should await wait response");
            }
            other => panic!("expected SupervisorPayload::WaitChild, got {other:?}"),
        }
    };

    let (result, ()) = tokio::join!(call, receive);
    assert_eq!(
        result.expect("running wait should be non-error JSON"),
        serde_json::json!({
            "child_id": "child-1",
            "status": "running",
            "ready": false
        })
    );
}

#[tokio::test]
async fn wait_child_agent_tool_returns_terminal_success_json_without_consuming_join_shape() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let tool = WaitChildAgentTool { sender };
    let call = tool.call(
        serde_json::json!({ "child_id": "child-1", "timeout_ms": 50 }),
        &CapabilityToken::default(),
    );
    let receive = async move {
        let message = receiver
            .recv()
            .await
            .expect("wait_child_agent should send one supervisor message");
        assert_eq!(message.priority, MessagePriority::Command);
        match message.payload {
            SupervisorPayload::WaitChild(child_id, timeout, result_tx) => {
                assert_eq!(child_id.0, "child-1");
                assert_eq!(timeout, Duration::from_millis(50));
                result_tx
                    .send(Ok(simulacra_runtime::WaitChildResult {
                        child_id: child_id.clone(),
                        agent_type: Some("researcher".into()),
                        status: "completed".into(),
                        ready: true,
                        terminal: Some(ChildTerminalResult {
                            child_id,
                            agent_type: "researcher".into(),
                            result: Ok(AgentLoopOutput {
                                exit_reason: ExitReason::Complete,
                                messages: vec![Message {
                                    role: Role::Assistant,
                                    content: "done".into(),
                                    tool_calls: vec![],
                                    tool_call_id: None,
                                }],
                                token_usage: TokenUsage {
                                    input_tokens: 11,
                                    output_tokens: 7,
                                },
                                used_turns: 1,
                                used_cost: Decimal::ZERO,
                            }),
                        }),
                    }))
                    .expect("wait_child_agent tool should await wait response");
            }
            other => panic!("expected SupervisorPayload::WaitChild, got {other:?}"),
        }
    };

    let (result, ()) = tokio::join!(call, receive);
    assert_eq!(
        result.expect("terminal wait should be non-error JSON"),
        serde_json::json!({
            "child_id": "child-1",
            "agent_type": "researcher",
            "status": "completed",
            "ready": true,
            "exit_reason": "completed",
            "message": "done",
            "token_usage": {
                "input_tokens": 11,
                "output_tokens": 7
            }
        })
    );
}

#[tokio::test]
async fn wait_child_agent_tool_returns_failed_terminal_json() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let tool = WaitChildAgentTool { sender };
    let call = tool.call(
        serde_json::json!({ "child_id": "child-1", "timeout_ms": 50 }),
        &CapabilityToken::default(),
    );
    let receive = async move {
        let message = receiver
            .recv()
            .await
            .expect("wait_child_agent should send one supervisor message");
        match message.payload {
            SupervisorPayload::WaitChild(child_id, _, result_tx) => {
                result_tx
                    .send(Ok(simulacra_runtime::WaitChildResult {
                        child_id: child_id.clone(),
                        agent_type: Some("researcher".into()),
                        status: "failed".into(),
                        ready: true,
                        terminal: Some(ChildTerminalResult {
                            child_id,
                            agent_type: "researcher".into(),
                            result: Err("boom".into()),
                        }),
                    }))
                    .expect("wait_child_agent tool should await wait response");
            }
            other => panic!("expected SupervisorPayload::WaitChild, got {other:?}"),
        }
    };

    let (result, ()) = tokio::join!(call, receive);
    assert_eq!(
        result.expect("failed terminal wait should be non-error JSON"),
        serde_json::json!({
            "child_id": "child-1",
            "agent_type": "researcher",
            "status": "failed",
            "ready": true,
            "exit_reason": "failed",
            "message": "boom",
            "token_usage": {
                "input_tokens": 0,
                "output_tokens": 0
            }
        })
    );
}

#[tokio::test]
async fn close_child_agent_tool_sends_command_and_returns_closed_status() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let tool = CloseChildAgentTool { sender };
    let call = tool.call(
        serde_json::json!({ "child_id": "child-1" }),
        &CapabilityToken::default(),
    );
    let receive = async move {
        let message = receiver
            .recv()
            .await
            .expect("close_child_agent should send one supervisor message");
        assert_eq!(message.priority, MessagePriority::Command);
        match message.payload {
            SupervisorPayload::CloseChild(child_id, result_tx) => {
                assert_eq!(child_id.0, "child-1");
                result_tx
                    .send(Ok(()))
                    .expect("close_child_agent tool should await close response");
            }
            other => panic!("expected SupervisorPayload::CloseChild, got {other:?}"),
        }
    };

    let (result, ()) = tokio::join!(call, receive);
    assert_eq!(
        result.expect("close should return closed status"),
        serde_json::json!({ "child_id": "child-1", "status": "closed" })
    );
}

#[tokio::test]
async fn s054_child_orchestration_tools_report_closed_supervisor_channels() {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    drop(receiver);

    let result = ChildStatusTool {
        sender: sender.clone(),
    }
    .call(
        serde_json::json!({ "child_id": "child-1" }),
        &CapabilityToken::default(),
    )
    .await;
    assert!(matches!(
        result,
        Err(ToolError::ExecutionFailed(message)) if message.contains("supervisor channel closed")
    ));

    let result = WaitChildAgentTool {
        sender: sender.clone(),
    }
    .call(
        serde_json::json!({ "child_id": "child-1", "timeout_ms": 0 }),
        &CapabilityToken::default(),
    )
    .await;
    assert!(matches!(
        result,
        Err(ToolError::ExecutionFailed(message)) if message.contains("supervisor channel closed")
    ));

    let result = CloseChildAgentTool { sender }
        .call(
            serde_json::json!({ "child_id": "child-1" }),
            &CapabilityToken::default(),
        )
        .await;
    assert!(matches!(
        result,
        Err(ToolError::ExecutionFailed(message)) if message.contains("supervisor channel closed")
    ));
}

// ── S023: Generic sub-agent tests ──────────────────────────────────────
