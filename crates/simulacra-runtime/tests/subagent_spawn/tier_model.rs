#[test]
fn generic_spawn_with_tier_uses_tier_model() {
    // When tier is specified and exists in config, the resolved model should come from tiers.
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-tier-1",
        "model": "claude-haiku-35-20241022",
        "choices": [{
            "message": { "role": "assistant", "content": "fast done" },
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

    let mut config = task_factory_config(CapabilitiesConfig {
        network: vec![],
        mcp: vec![],
        shell: false,
        javascript: false,
        python: false,
        paths_read: vec![],
        paths_write: vec![],

        skill_patterns: vec![],

        memory: None,
    });
    config
        .tiers
        .insert("fast".into(), "claude-haiku-35-20241022".into());

    let factory = AgentTaskFactory {
        config,
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        allowed_mcp_servers: None,
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: None,
            acp_child_runtime: None,
    };

    let mut spawn = generic_spawn_config(
        "child-tier-1",
        "parent-agent",
        "You are a fast helper.",
        child_budget(32, 1, 0),
    );
    spawn.tier = Some("fast".into());

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("generic child with tier should complete");

    let request = server.first_request_json();
    assert_eq!(
        request["model"], "claude-haiku-35-20241022",
        "generic child with tier='fast' should use the model from tiers config"
    );
    assert_eq!(output.exit_reason, ExitReason::Complete);
}

#[test]
fn generic_spawn_without_tier_inherits_parent_model() {
    // When no tier is specified, the child should inherit the parent's model.
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-notier-1",
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
        allowed_mcp_servers: None,
        supervisor_sender: None,
        parent_model: "my-specific-parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: None,
            acp_child_runtime: None,
    };

    let spawn = generic_spawn_config(
        "child-notier-1",
        "parent-agent",
        "You are a helper.",
        child_budget(32, 1, 0),
    );

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("generic child without tier should complete");

    let request = server.first_request_json();
    assert_eq!(
        request["model"], "my-specific-parent-model",
        "generic child without tier should inherit parent_model"
    );
    assert_eq!(output.exit_reason, ExitReason::Complete);
}

#[tokio::test]
async fn generic_spawn_consumes_parent_budget() {
    // Generic spawn should still increment parent's used_sub_agents.
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    let spawn = generic_spawn_config(
        "child-budget-1",
        "parent-agent",
        "You are a helper.",
        child_budget(10, 1, 1),
    );

    supervisor
        .spawn_agent(spawn)
        .expect("generic spawn should succeed");
    factory.wait_for_completed(1).await;

    assert_eq!(
        supervisor.parent_budget().used_sub_agents,
        1,
        "generic spawn should increment parent used_sub_agents"
    );
}

#[tokio::test]
async fn configured_spawn_still_works() {
    // Regression test: configured spawn (with agent_type) should still work
    // after introducing the generic branch.
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    supervisor
        .spawn_agent(spawn_config_with_agent_type(
            "child-configured-1",
            "parent-agent",
            "researcher",
            child_budget(10, 1, 1),
        ))
        .expect("configured spawn should still work");
    factory.wait_for_completed(1).await;

    let started = factory.inner.started.lock().unwrap().clone();
    let snapshot = started.first().expect("factory should record the spawn");
    assert_eq!(snapshot.agent_type, "researcher");
    assert_eq!(snapshot.task, "delegate task");
    assert_eq!(
        supervisor.parent_budget().used_sub_agents,
        1,
        "configured spawn should still increment budget"
    );
}

#[tokio::test]
async fn generic_spawn_with_unknown_tier_errors() {
    // When tiers config is populated, an unknown tier name should produce an error
    // listing the valid tier names.
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let mut tiers = TierMap::default();
    tiers.insert("reasoning".to_string(), "claude-opus-4-6".to_string());
    tiers.insert("balanced".to_string(), "claude-sonnet-4-6".to_string());
    tiers.insert("fast".to_string(), "claude-haiku-4-5-20251001".to_string());

    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec![],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers,
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    };

    let result = tool
        .call(
            serde_json::json!({
                "system_prompt": "You are a helper.",
                "task": "do something",
                "budget": {
                    "max_tokens": 10,
                    "max_turns": 1,
                    "max_cost": "1",
                    "max_sub_agents": 0
                },
                "tier": "turbo"
            }),
            &CapabilityToken::default(),
        )
        .await;

    match result {
        Err(ToolError::ExecutionFailed(msg)) => {
            assert!(
                msg.contains("unknown tier 'turbo'"),
                "error should mention the unknown tier name: {msg}"
            );
            // The error should list the valid tier names
            assert!(
                msg.contains("reasoning") || msg.contains("balanced") || msg.contains("fast"),
                "error should list valid tiers: {msg}"
            );
        }
        other => panic!("unknown tier should return ExecutionFailed, got {other:?}"),
    }
}

#[test]
fn default_system_prompt_describes_current_sandbox_affordances() {
    assert!(DEFAULT_SYSTEM_PROMPT.contains("fresh JS global/context"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("simulacra:path"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("simulacra:crypto"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("Cwd and env vars persist"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("node -"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("python -"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("/proc/mailbox/<filename>"));
    assert!(!DEFAULT_SYSTEM_PROMPT.contains("persistent QuickJS context"));
    assert!(!DEFAULT_SYSTEM_PROMPT.contains("No `cd`"));
}

#[test]
fn generic_spawn_empty_system_prompt_uses_default() {
    // When system_prompt is "" (empty string), the factory should fall back to
    // DEFAULT_SYSTEM_PROMPT rather than sending an empty system prompt to the provider.
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-empty-sp-1",
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
        allowed_mcp_servers: None,
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: None,
            acp_child_runtime: None,
    };

    // Empty system_prompt — should fall back to DEFAULT_SYSTEM_PROMPT
    let spawn = generic_spawn_config(
        "child-empty-sp-1",
        "parent-agent",
        "",
        child_budget(32, 1, 0),
    );

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("generic child with empty system_prompt should complete");

    let request = server.first_request_json();
    let sent_prompt = request["messages"][0]["content"].as_str().unwrap_or("");
    assert!(
        !sent_prompt.is_empty(),
        "empty system_prompt should fall back to DEFAULT_SYSTEM_PROMPT, not send empty string"
    );
    assert!(
        sent_prompt.contains("You are a helpful AI assistant"),
        "fallback should be DEFAULT_SYSTEM_PROMPT, got: {}",
        &sent_prompt[..sent_prompt.len().min(80)]
    );
    assert!(
        sent_prompt.contains("fresh JS global/context"),
        "fallback prompt should tell child agents js_exec is single-shot, got: {sent_prompt}"
    );
    assert!(
        sent_prompt.contains("Cwd and env vars persist"),
        "fallback prompt should advertise persistent shell cwd/env, got: {sent_prompt}"
    );
    assert!(
        sent_prompt.contains("node -") && sent_prompt.contains("python -"),
        "fallback prompt should advertise stdin interpreter aliases, got: {sent_prompt}"
    );
    assert!(
        sent_prompt.contains("/proc/mailbox/<filename>"),
        "fallback prompt should tell child agents where to write artifacts, got: {sent_prompt}"
    );
    assert!(
        !sent_prompt.contains("persistent QuickJS context") && !sent_prompt.contains("No `cd`"),
        "fallback prompt should not contain stale sandbox affordance guidance, got: {sent_prompt}"
    );
    assert_eq!(output.exit_reason, ExitReason::Complete);
}

#[tokio::test]
async fn generic_spawn_empty_agent_type_string_errors() {
    // When agent_type is "" (empty string), it should be treated as None,
    // so without system_prompt the call errors with "either agent_type or system_prompt is required".
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
                "agent_type": "",
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
                "empty agent_type should be treated as None: {msg}"
            );
        }
        other => {
            panic!("empty agent_type string should be treated as None and error, got {other:?}")
        }
    }
}
