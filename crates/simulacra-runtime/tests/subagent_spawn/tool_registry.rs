#[test]
fn configured_spawn_capable_child_registry_includes_all_child_control_tools() {
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-child-controls-1",
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
    let parent_cap = CapabilityToken {
        spawn_placements: vec!["reviewer".into()],
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
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
    let reviewer_placement = config
        .child_placements
        .get("researcher")
        .cloned()
        .expect("researcher fixture should exist");
    config
        .child_placements
        .insert("reviewer".into(), reviewer_placement);
    config
        .child_placements
        .get_mut("researcher")
        .expect("researcher fixture should exist")
        .allowed_child_placements = vec!["reviewer".into()];
    let factory = AgentTaskFactory {
        config,
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: parent_cap,
        allowed_mcp_servers: None,
        supervisor_sender: Some(tx),
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: None,
            acp_child_runtime: None,
    };

    let spawn = spawn_config("child-controls-1", "parent-agent", child_budget(32, 1, 1));

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("configured child should complete");
    assert_eq!(output.exit_reason, ExitReason::Complete);

    let request = server.first_request_json();
    let tool_names: BTreeSet<String> = request
        .get("tools")
        .and_then(|v| v.as_array())
        .expect("configured child request should include tool definitions")
        .iter()
        .map(|tool| {
            tool.pointer("/function/name")
                .and_then(|v| v.as_str())
                .expect("tool definition should include function.name")
                .to_string()
        })
        .collect();

    for expected in [
        "spawn_agent",
        "join_child_agent",
        "cancel_child_agent",
        "steer_child_agent",
        "child_status",
        "list_child_agents",
        "wait_child_agent",
        "close_child_agent",
    ] {
        assert!(
            tool_names.contains(expected),
            "spawn-capable configured child should include {expected}; tools were: {tool_names:?}"
        );
    }
    for host_only_name in [
        "inspect_child_result",
        "InspectChildResult",
        "inspect_children",
        "InspectChildren",
    ] {
        assert!(
            !tool_names.contains(host_only_name),
            "host-only inspection must not be registered in the child model tool catalog: {tool_names:?}"
        );
    }
}

fn configured_child_catalog_config(
    vfs: &Arc<dyn VirtualFs>,
    declared_server: &str,
    child_mcp_capability: &str,
    endpoint: String,
) -> SimulacraConfig {
    vfs.mkdir("/skills").expect("skills root should be created");
    vfs.mkdir("/skills/repo-work")
        .expect("skill directory should be created");
    vfs.write(
        "/skills/repo-work/SKILL.md",
        format!(
            "---\nname: repo-work\ndescription: Repository work.\nmcp_servers:\n  - {declared_server}\n---\n\nUse the repository catalog.\n"
        )
        .as_bytes(),
    )
    .expect("skill should be written to the inherited VFS");

    let mut config = task_factory_config(CapabilitiesConfig {
        network: vec![],
        mcp: vec![child_mcp_capability.into()],
        shell: false,
        javascript: false,
        python: false,
        paths_read: vec!["/skills/**".into()],
        paths_write: vec![],
        skill_patterns: vec!["skill:repo-work".into()],
        memory: None,
    });
    config
        .child_placements
        .get_mut("researcher")
        .expect("researcher fixture should exist")
        .skills = vec!["repo-work".into()];
    config.mcp = Some(simulacra_config::McpConfig {
        servers: vec![simulacra_config::McpServerConfig {
            name: declared_server.into(),
            transport: Some("http".into()),
            url: Some(endpoint),
            module: None,
            env: None,
            network: vec![],
            wasi: None,
        }],
    });
    config
}

#[test]
fn configured_native_child_builds_its_own_stable_skill_mcp_catalog_without_connecting() {
    let _env_lock = openai_env_guard();
    let provider = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-child-mcp-catalog",
        "model": "child-model",
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &provider.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &provider.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");
    let mcp_probe = TcpListener::bind("127.0.0.1:0").expect("MCP probe should bind");
    mcp_probe
        .set_nonblocking(true)
        .expect("MCP probe should be nonblocking");
    let mcp_url = format!("http://{}", mcp_probe.local_addr().expect("probe address"));

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace").expect("workspace should be created");
    let config = configured_child_catalog_config(&vfs, "github", "mcp:github:*", mcp_url);
    let factory = AgentTaskFactory {
        config,
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal: Arc::new(InMemoryJournalStorage::new()),
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken {
            mcp_tools: vec!["mcp:github:*".into()],
            skill_patterns: vec!["skill:repo-work".into()],
            ..Default::default()
        },
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
    let spawn = spawn_config("child-mcp-catalog", "parent-agent", child_budget(32, 1, 0));

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("configured child should complete without eagerly connecting MCP");

    assert!(
        matches!(mcp_probe.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "child catalog construction must keep configured MCP descriptors disconnected"
    );
    let request = provider.first_request_json();
    let tool_names: BTreeSet<_> = request["tools"]
        .as_array()
        .expect("child request should include tools")
        .iter()
        .filter_map(|tool| tool.pointer("/function/name").and_then(|name| name.as_str()))
        .collect();
    for expected in ["Skill", "mcp_search", "mcp_call"] {
        assert!(tool_names.contains(expected), "missing {expected}: {tool_names:?}");
    }
}

#[test]
fn configured_native_child_prevalidates_skill_dependencies_with_attenuated_capability() {
    let _env_lock = openai_env_guard();
    let provider = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "unexpected-provider-call",
        "model": "child-model",
        "choices": [{"message":{"role":"assistant","content":"unexpected"},"finish_reason":"stop"}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &provider.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &provider.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");
    let mcp_probe = TcpListener::bind("127.0.0.1:0").expect("MCP probe should bind");
    mcp_probe.set_nonblocking(true).expect("probe nonblocking");
    let endpoint = format!("http://{}", mcp_probe.local_addr().expect("probe address"));
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace").expect("workspace should be created");
    let config = configured_child_catalog_config(&vfs, "linear", "mcp:github:*", endpoint);
    let factory = AgentTaskFactory {
        config,
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal: Arc::new(InMemoryJournalStorage::new()),
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken {
            mcp_tools: vec!["mcp:github:*".into(), "mcp:linear:*".into()],
            skill_patterns: vec!["skill:repo-work".into()],
            ..Default::default()
        },
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
    let spawn = spawn_config("child-mcp-denied", "parent-agent", child_budget(32, 1, 0));

    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect_err("child capability must reject its denied skill dependency");
    let message = error.to_string();
    assert!(message.contains("repo-work") && message.contains("linear"));
    assert!(matches!(mcp_probe.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
}

#[test]
fn configured_native_child_enforces_tenant_mcp_allowlist_before_network_access() {
    let _env_lock = openai_env_guard();
    let provider = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "unexpected-provider-call",
        "model": "child-model",
        "choices": [{"message":{"role":"assistant","content":"unexpected"},"finish_reason":"stop"}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &provider.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &provider.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");
    let mcp_probe = TcpListener::bind("127.0.0.1:0").expect("MCP probe should bind");
    mcp_probe.set_nonblocking(true).expect("probe nonblocking");
    let endpoint = format!("http://{}", mcp_probe.local_addr().expect("probe address"));
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace").expect("workspace should be created");
    let mut config =
        configured_child_catalog_config(&vfs, "github", "mcp:github:*", endpoint);
    config.tenants.insert(
        "tenant-a".into(),
        simulacra_config::TenantConfig {
            agent_type: "researcher".into(),
            integrations: None,
            mcp_servers: Some(vec!["linear".into()]),
        },
    );
    let factory = AgentTaskFactory {
        config,
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal: Arc::new(InMemoryJournalStorage::new()),
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken {
            mcp_tools: vec!["mcp:github:*".into()],
            skill_patterns: vec!["skill:repo-work".into()],
            ..Default::default()
        },
        allowed_mcp_servers: Some(vec!["linear".into()]),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: None,
        acp_child_runtime: None,
    };
    let spawn = spawn_config("tenant-child-mcp-denied", "parent-agent", child_budget(32, 1, 0));

    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect_err("tenant-excluded MCP dependency must reject configured child construction");
    let message = error.to_string();
    assert!(message.contains("repo-work") && message.contains("github"));
    assert!(matches!(mcp_probe.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
    assert_eq!(
        provider
            .requests
            .lock()
            .expect("provider request mutex")
            .len(),
        0,
        "tenant dependency validation must fail before child provider execution"
    );
}

#[tokio::test]
async fn placement_spawn_aborts_when_subagent_spawned_journal_append_fails() {
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    supervisor.set_journal_storage(Arc::new(FailingAppendJournal));

    let err = supervisor
        .spawn_agent(spawn_config_with_placement(
            "child-journal-fail",
            "parent-agent",
            "researcher",
            child_budget(10, 1, 1),
        ))
        .expect_err("placement spawn must fail before execution if spawn journaling fails");

    assert!(
        matches!(
            err,
            RuntimeError::JournalAppendFailed {
                entry_kind: "SubAgentSpawned",
                ..
            }
        ),
        "journal append failure should be surfaced as JournalAppendFailed, got {err:?}"
    );
    assert_eq!(
        factory.started_count(),
        0,
        "child task must not start if the parent spawn audit entry is missing"
    );
    assert_eq!(
        supervisor.parent_budget().used_sub_agents,
        0,
        "rejected spawn must not consume parent sub-agent budget"
    );
}
