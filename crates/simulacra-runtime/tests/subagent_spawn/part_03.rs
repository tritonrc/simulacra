#[tokio::test]
async fn child_internal_messages_are_not_appended_to_parent_conversation_history() {
    // Exercise the real SpawnAgentTool path. The child output contains multiple
    // messages (system, user, assistant), but the parent should only see the
    // single JSON tool result — not the child's internal conversation.
    let child_output = AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![
            Message {
                role: Role::System,
                content: "child system".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: "child task".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: Role::Assistant,
                content: "child result".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ],
        token_usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
        used_turns: 1,
        used_cost: Decimal::ZERO,
    };

    let (result, _captured) = run_spawn_tool_call(
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
        &["researcher"],
        Ok(child_output),
    )
    .await;

    // The real SpawnAgentTool returns a single JSON value. The child's internal
    // System/User/Assistant messages are NOT surfaced — only the terminal summary.
    let value = result.expect("spawn should succeed");
    assert_eq!(
        value.get("message").and_then(serde_json::Value::as_str),
        Some("child result"),
        "parent should see only the child's final assistant message as a summary, not internal messages"
    );
    // Verify the result is a single flat JSON object, not a list of messages.
    assert!(
        value.is_object() && !value.is_array(),
        "spawn_agent should return a single JSON object, not an array of child messages"
    );
}

#[test]
fn agent_task_factory_runs_a_real_child_agent_loop_with_the_child_prompt_and_model() {
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-1",
        "model": "child-model",
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: false,
            javascript: false,
            python: false,
            paths_read: vec!["/workspace/**".into()],
            paths_write: vec![],

            skill_patterns: vec![],

            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal: Arc::clone(&journal),
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };
    let spawn = spawn_config("child-1", "parent-agent", child_budget(32, 1, 0));

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(
            spawn.clone(),
            CancellationToken::new(Duration::from_secs(1)),
        ))
        .expect("child task should complete");

    assert_eq!(
        output.messages.first().map(|message| message.role.clone()),
        Some(Role::System)
    );
    assert_eq!(
        output
            .messages
            .first()
            .map(|message| message.content.as_str()),
        Some("You are the child researcher."),
        "child execution should run through AgentLoop::run(task) so the configured system prompt is present"
    );
    assert_eq!(
        output.messages.get(1).map(|message| message.role.clone()),
        Some(Role::User)
    );
    assert_eq!(
        output
            .messages
            .get(1)
            .map(|message| message.content.as_str()),
        Some("delegate task"),
        "child execution should preserve the delegated task as the child user turn"
    );

    let request = server.first_request_json();
    assert_eq!(request["model"], "child-model");
    assert_eq!(
        request["messages"][0]["content"],
        "You are the child researcher."
    );

    let child_entries = journal
        .read_all(&spawn.agent_id)
        .expect("child journal should be readable");
    assert!(
        !child_entries.is_empty()
            && child_entries
                .iter()
                .all(|entry| entry.agent_id == spawn.agent_id),
        "child journal entries should be written under the child agent_id so they correlate through child_id"
    );
}

#[test]
fn agent_task_factory_applies_child_cell_and_tool_hooks_before_provider_call() {
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-1",
        "model": "child-model",
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let observed_configured_cell = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_for_registrar = Arc::clone(&observed_configured_cell);

    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: false,
            javascript: false,
            python: false,
            paths_read: vec!["/workspace/**".into()],
            paths_write: vec![],
            skill_patterns: vec![],
            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: Some(Arc::new(|cell: &mut simulacra_sandbox::AgentCell| {
            cell.tenant_integrations = vec!["toy-saas".to_string()];
        })),
        child_tool_registrar: Some(Arc::new(move |registry, cell| {
            observed_for_registrar.store(
                cell.tenant_integrations == vec!["toy-saas".to_string()],
                Ordering::SeqCst,
            );
            registry.register(Box::new(ExtraProbeTool))?;
            Ok(())
        })),
    };
    let spawn = spawn_config("child-hooks-1", "parent-agent", child_budget(32, 1, 0));

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("child task should complete");

    assert!(
        observed_configured_cell.load(Ordering::SeqCst),
        "child tool registration should see the AgentCell after caller-specific configuration"
    );
    let request = server.first_request_json();
    let tools = request
        .get("tools")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        tools.iter().any(|t| {
            t.pointer("/function/name")
                .and_then(|v| v.as_str())
                .map(|name| name == "child_extra_probe")
                .unwrap_or(false)
        }),
        "child provider call should include caller-registered child tools"
    );
}

#[test]
fn agent_task_factory_intersects_child_type_capability_with_the_spawn_override() {
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

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: true,
            javascript: false,
            python: false,
            paths_read: vec!["/workspace/**".into()],
            paths_write: vec!["/workspace/**".into()],

            skill_patterns: vec![],

            memory: None,
        }),
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
    let mut spawn = spawn_config("child-1", "parent-agent", child_budget(32, 1, 0));
    spawn.capability = Some(CapabilityToken::default());

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
        "effective child capability should be the intersection of child type config and the attenuated spawn capability override"
    );
}

#[test]
fn widened_child_capabilities_are_rejected_before_the_child_task_starts() {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    let child = CapabilityToken {
        shell: true,
        ..Default::default()
    };

    let result = supervisor.spawn_agent(SpawnConfig {
        agent_id: AgentId("child-1".into()),
        parent_id: AgentId("parent-agent".into()),
        capability: Some(child),
        budget: child_budget(10, 1, 1),
        restart_strategy: RestartStrategy::LetCrash,
        agent_type: Some(String::new()),
        task: String::new(),
        system_prompt: None,
        tier: None,
        resolved_tier: None,
    });

    assert!(
        matches!(result, Err(RuntimeError::CapabilityViolation(_))),
        "capability widening must be rejected before the child task starts"
    );
    assert_eq!(factory.started_count(), 0);
}

#[test]
fn child_may_spawn_descendants_only_from_its_own_remaining_budget() {
    let child_supervisor = AgentSupervisor::new(
        CapabilityToken {
            spawn_types: vec!["reviewer".into()],
            ..Default::default()
        },
        child_budget(10, 2, 1),
    );
    let mut child_supervisor = child_supervisor;

    let result = child_supervisor.spawn_agent(spawn_config(
        "grandchild-1",
        "child-1",
        child_budget(11, 1, 1),
    ));

    assert!(
        matches!(result, Err(RuntimeError::BudgetExhausted(_))),
        "descendant reservations should be enforced against the child's own remaining budget"
    );
}

#[test]
fn parent_replay_reuses_recorded_spawn_agent_tool_result_without_a_live_child_run() {
    let live_calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(SummarySpawnTool {
            live_calls: Arc::clone(&live_calls),
        }))
        .expect("test tool registration should succeed");
    let provider = FakeProvider::new(vec![]);
    let replay = vec![
        replay_entry("parent-agent", JournalEntryKind::TurnStart),
        replay_entry(
            "parent-agent",
            JournalEntryKind::LlmRequest {
                model: "test-model".into(),
                message_count: 1,
            },
        ),
        replay_entry(
            "parent-agent",
            JournalEntryKind::LlmResponse {
                model: "test-model".into(),
                token_usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                finish_reason: "ToolUse".into(),
                assistant_message: Some(Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls: vec![ToolCallMessage {
                        id: "call-1".into(),
                        name: "spawn_agent".into(),
                        arguments: serde_json::json!({}),
                    }],
                    tool_call_id: None,
                }),
            },
        ),
        replay_entry(
            "parent-agent",
            JournalEntryKind::ToolCall {
                tool_call_id: Some("call-1".into()),
                tool_name: "spawn_agent".into(),
                arguments: serde_json::json!({}),
            },
        ),
        replay_entry(
            "parent-agent",
            JournalEntryKind::ToolResult {
                tool_call_id: Some("call-1".into()),
                tool_name: "spawn_agent".into(),
                content: r#"{"child_id":"child-1","agent_type":"researcher","exit_reason":"completed","message":"done","token_usage":{"input_tokens":3,"output_tokens":2}}"#.into(),
                is_error: false,
            },
        ),
    ];
    let mut loop_ = build_loop(provider, tools, Some(replay));
    let mut messages = vec![Message {
        role: Role::User,
        content: "delegate".into(),
        tool_calls: vec![],
        tool_call_id: None,
    }];

    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(loop_.run_single_turn(&mut messages))
        .expect("replayed turn should succeed");

    assert!(
        matches!(result, TurnResult::ToolCallsProcessed { .. }),
        "replay should preserve the parent-visible spawn_agent tool result"
    );
    assert_eq!(
        live_calls.load(Ordering::SeqCst),
        0,
        "replay should not invoke a live child tool call when ToolResult is already journaled"
    );
}
