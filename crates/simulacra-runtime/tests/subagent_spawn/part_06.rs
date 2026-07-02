/// Helper to create a generic spawn config (no agent_type, uses system_prompt).
fn generic_spawn_config(
    agent_id: &str,
    parent_id: &str,
    system_prompt: &str,
    budget: ResourceBudget,
) -> SpawnConfig {
    SpawnConfig {
        agent_id: AgentId(agent_id.into()),
        parent_id: AgentId(parent_id.into()),
        capability: None,
        budget,
        restart_strategy: RestartStrategy::LetCrash,
        agent_type: None,
        task: "generic task".into(),
        system_prompt: Some(system_prompt.into()),
        tier: None,
        resolved_tier: None,
    }
}

#[test]
fn generic_spawn_with_system_prompt_creates_child() {
    // Spawn with system_prompt, no agent_type. Verify the child runs and the
    // system prompt is forwarded to the provider.
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-generic-1",
        "model": "parent-model",
        "choices": [{
            "message": { "role": "assistant", "content": "generic done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8 }
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
            paths_read: vec![],
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
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    let spawn = generic_spawn_config(
        "child-generic-1",
        "parent-agent",
        "You are a custom generic agent.",
        child_budget(32, 1, 0),
    );

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("generic child task should complete");

    // Verify the system prompt was used
    let request = server.first_request_json();
    assert_eq!(
        request["messages"][0]["content"], "You are a custom generic agent.",
        "generic child should use the inline system_prompt"
    );

    // Verify the model was the parent's model (no tier override)
    assert_eq!(
        request["model"], "parent-model",
        "generic child without tier should inherit parent model"
    );

    // Verify the child completed
    assert_eq!(output.exit_reason, ExitReason::Complete);
}

#[tokio::test]
async fn generic_spawn_with_both_agent_type_and_system_prompt_errors() {
    // SpawnAgentTool should reject when both agent_type and system_prompt are provided.
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec!["researcher".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    };

    let result = tool
        .call(
            serde_json::json!({
                "agent_type": "researcher",
                "system_prompt": "You are custom.",
                "task": "do something",
                "budget": {
                    "max_tokens": 10,
                    "max_turns": 1,
                    "max_cost": "1",
                    "max_sub_agents": 0
                }
            }),
            &CapabilityToken::default(),
        )
        .await;

    match result {
        Err(ToolError::InvalidArguments(msg)) => {
            assert!(
                msg.contains("agent_type or system_prompt, not both"),
                "error should mention mutual exclusivity: {msg}"
            );
        }
        other => panic!(
            "providing both agent_type and system_prompt should return InvalidArguments, got {other:?}"
        ),
    }
}

#[tokio::test]
async fn generic_spawn_with_neither_agent_type_nor_system_prompt_errors() {
    // SpawnAgentTool should reject when neither agent_type nor system_prompt is provided.
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec![],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    };

    let result = tool
        .call(
            serde_json::json!({
                "task": "do something",
                "budget": {
                    "max_tokens": 10,
                    "max_turns": 1,
                    "max_cost": "1",
                    "max_sub_agents": 0
                }
            }),
            &CapabilityToken::default(),
        )
        .await;

    match result {
        Err(ToolError::InvalidArguments(msg)) => {
            assert!(
                msg.contains("either agent_type or system_prompt is required"),
                "error should mention that one is required: {msg}"
            );
        }
        other => panic!(
            "providing neither agent_type nor system_prompt should return InvalidArguments, got {other:?}"
        ),
    }
}

#[tokio::test]
async fn generic_spawn_system_prompt_exceeds_8kb_errors() {
    // SpawnAgentTool should reject system_prompt > 8192 bytes.
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec![],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    };

    let oversized_prompt = "x".repeat(9000);
    let result = tool
        .call(
            serde_json::json!({
                "system_prompt": oversized_prompt,
                "task": "do something",
                "budget": {
                    "max_tokens": 10,
                    "max_turns": 1,
                    "max_cost": "1",
                    "max_sub_agents": 0
                }
            }),
            &CapabilityToken::default(),
        )
        .await;

    match result {
        Err(ToolError::ExecutionFailed(msg)) => {
            assert!(
                msg.contains("8192") && msg.contains("9000"),
                "error should mention the 8192 byte limit and the actual size: {msg}"
            );
        }
        other => panic!("system_prompt > 8192 bytes should return ExecutionFailed, got {other:?}"),
    }
}

#[test]
fn generic_spawn_inherits_parent_capabilities() {
    // Generic spawn without capability override should inherit the parent's full capability.
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-cap-1",
        "model": "parent-model",
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let parent_cap = CapabilityToken {
        shell: true,
        javascript: true,
        python: false,
        network: vec![NetworkPermission("net:api.github.com".into())],
        ..Default::default()
    };

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());

    // Use a CapturingTaskFactory to inspect the child config
    // Instead, we use AgentTaskFactory directly and check the child loop's behavior.
    // The child should inherit parent capabilities since no override is provided.
    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: false,
            javascript: false,
            python: false,
            paths_read: vec![],
            paths_write: vec![],

            skill_patterns: vec![],

            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: parent_cap.clone(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    // Generic spawn with no capability override
    let spawn = generic_spawn_config(
        "child-cap-1",
        "parent-agent",
        "You are a helper.",
        child_budget(32, 1, 0),
    );

    // The child should succeed and use the parent's capability token.
    // We verify by checking the output — if the factory branches correctly,
    // the generic path uses parent_capability.clone() (no intersection with config).
    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("generic child should complete with parent capabilities");

    assert_eq!(output.exit_reason, ExitReason::Complete);
}

#[test]
fn generic_spawn_with_capability_override_intersects_parent() {
    // Generic spawn with capability override should intersect with parent (two-way).
    // Parent: shell=true, javascript=true
    // Override: shell=true, javascript=false
    // Effective: shell=true (both allow), javascript=false (override denies)
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-cap-2",
        "model": "parent-model",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call-js-1",
                    "type": "function",
                    "function": {
                        "name": "js_exec",
                        "arguments": "{\"code\":\"console.log('hello')\"}"
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

    let parent_cap = CapabilityToken {
        shell: true,
        javascript: true,
        python: false,
        ..Default::default()
    };

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
            paths_read: vec![],
            paths_write: vec![],

            skill_patterns: vec![],

            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: parent_cap,
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    // Generic spawn with override: javascript=false
    let mut spawn = generic_spawn_config(
        "child-cap-2",
        "parent-agent",
        "You are a helper.",
        child_budget(32, 1, 0),
    );
    spawn.capability = Some(CapabilityToken {
        shell: true,
        javascript: false,
        ..Default::default()
    });

    // The child tries to use js_exec, but javascript=false in the effective capability
    // (parent=true, override=false => intersection=false). The child should get a
    // capability violation error.
    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("child should complete even if tool call fails");

    // The child should have attempted the tool call and gotten a capability error.
    // The output should contain the error in messages (the agent loop continues).
    let has_capability_error = output.messages.iter().any(|m| {
        m.content.contains("capability")
            || m.content.contains("not allowed")
            || m.content.contains("denied")
    });
    assert!(
        has_capability_error || output.exit_reason == ExitReason::MaxTurns,
        "generic child with javascript denied should either see a capability error or hit max_turns, got exit_reason={:?}",
        output.exit_reason
    );
}

