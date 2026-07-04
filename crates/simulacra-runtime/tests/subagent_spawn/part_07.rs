#[test]
fn generic_spawn_tool_registry_includes_all_builtins_and_excludes_spawn_agent() {
    // Generic children are full leaf workers: they get the standard built-ins
    // but never the delegation tool.
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-nospawn-1",
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

    // Give the parent spawn_types — but generic agents should still not get spawn_agent
    let parent_cap = CapabilityToken {
        spawn_types: vec!["researcher".into()],
        ..Default::default()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(1);
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
        supervisor_sender: Some(tx),
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    let spawn = generic_spawn_config(
        "child-nospawn-1",
        "parent-agent",
        "You are a leaf worker.",
        child_budget(32, 1, 0),
    );

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("generic child should complete");

    // Verify the request sent to the provider includes all standard built-ins
    // and does NOT include spawn_agent.
    let request = server.first_request_json();
    let tool_names: BTreeSet<String> = request
        .get("tools")
        .and_then(|v| v.as_array())
        .expect("generic child request should include tool definitions")
        .iter()
        .map(|tool| {
            tool.pointer("/function/name")
                .and_then(|v| v.as_str())
                .expect("tool definition should include function.name")
                .to_string()
        })
        .collect();
    let expected_builtins: BTreeSet<String> = [
        "file_read",
        "file_write",
        "apply_patch",
        "shell_exec",
        "js_exec",
        "list_dir",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    assert_eq!(
        tool_names, expected_builtins,
        "generic child tool registry should contain exactly the standard built-ins and no spawn_agent"
    );
    let has_spawn_tool = tool_names.iter().any(|name| name.as_str() == "spawn_agent");
    assert!(
        !has_spawn_tool,
        "generic child agent should NOT have spawn_agent tool registered — generic agents are leaf workers"
    );
    assert_eq!(output.exit_reason, ExitReason::Complete);
}

#[tokio::test]
async fn generic_spawn_parent_max_sub_agents_zero_remains_unlimited() {
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut parent_budget = default_budget();
    parent_budget.max_sub_agents = 0;
    parent_budget.used_sub_agents = 17;

    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        parent_budget,
        Arc::new(factory.clone()),
    );

    supervisor
        .spawn_agent(generic_spawn_config(
            "child-generic-unlimited-subagents",
            "parent-agent",
            "You are a leaf worker.",
            child_budget(10, 1, 0),
        ))
        .expect("generic spawn should accept parent max_sub_agents = 0 as unlimited");
    factory.wait_for_completed(1).await;

    assert_eq!(
        supervisor.parent_budget().used_sub_agents,
        18,
        "accepted generic spawn should still increment usage under an unlimited parent budget"
    );
}

#[tokio::test]
async fn generic_subagent_spawned_journal_records_full_system_prompt_for_audit() {
    let parent_id = AgentId("parent-agent".into());
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    supervisor.set_journal_storage(Arc::clone(&journal));

    let system_prompt =
        "You are a generic audit worker. Preserve this exact prompt in the parent journal.";
    supervisor
        .spawn_agent(generic_spawn_config(
            "child-generic-audit",
            &parent_id.0,
            system_prompt,
            child_budget(10, 1, 0),
        ))
        .expect("generic spawn should be accepted");
    factory.wait_for_completed(1).await;

    let spawned_entry = journal
        .read_all(&parent_id)
        .expect("parent journal should be readable")
        .into_iter()
        .find_map(|entry| match entry.entry {
            JournalEntryKind::SubAgentSpawned { .. } => Some(entry.entry),
            _ => None,
        })
        .expect("generic spawn should append SubAgentSpawned");
    let spawned_json =
        serde_json::to_value(&spawned_entry).expect("journal entry should serialize to JSON");

    assert_eq!(
        spawned_json.get("agent_type").and_then(|v| v.as_str()),
        Some("generic"),
        "generic SubAgentSpawned entries should label agent_type as generic"
    );
    assert_eq!(
        spawned_json.get("system_prompt").and_then(|v| v.as_str()),
        Some(system_prompt),
        "generic SubAgentSpawned entries should include the full inline system_prompt for audit"
    );
}

#[tokio::test]
async fn generic_spawn_aborts_when_subagent_spawned_journal_append_fails() {
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    supervisor.set_journal_storage(Arc::new(FailingAppendJournal));

    let err = supervisor
        .spawn_agent(generic_spawn_config(
            "child-generic-journal-fail",
            "parent-agent",
            "You are an audit-sensitive worker.",
            child_budget(10, 1, 0),
        ))
        .expect_err("generic spawn must fail before child execution if spawn journaling fails");

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

#[tokio::test]
async fn generic_create_agent_span_records_generic_spawn_mode_and_explicit_tier() {
    let (_, spans, _) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            CapabilityToken::default(),
            default_budget(),
            Arc::new(NoopFactory),
        );
        let mut spawn = generic_spawn_config(
            "child-generic-fast",
            "parent-agent",
            "You are a fast leaf worker.",
            child_budget(10, 1, 0),
        );
        spawn.tier = Some("fast".into());
        supervisor
            .spawn_agent(spawn)
            .expect("generic spawn with explicit tier should succeed");
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "create_agent"
                && span
                    .fields
                    .get("simulacra.agent.spawn_mode")
                    .map(String::as_str)
                    == Some("generic")
                && span.fields.get("simulacra.agent.tier").map(String::as_str) == Some("fast")
        }),
        "generic create_agent span should record spawn_mode=generic and the explicit resolved tier"
    );
}

#[tokio::test]
async fn generic_create_agent_span_labels_missing_tier_as_balanced_fallback() {
    let (_, spans, _) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            CapabilityToken::default(),
            default_budget(),
            Arc::new(NoopFactory),
        );
        supervisor
            .spawn_agent(generic_spawn_config(
                "child-generic-balanced",
                "parent-agent",
                "You are a balanced leaf worker.",
                child_budget(10, 1, 0),
            ))
            .expect("generic spawn without explicit tier should succeed");
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "create_agent"
                && span
                    .fields
                    .get("simulacra.agent.spawn_mode")
                    .map(String::as_str)
                    == Some("generic")
                && span.fields.get("simulacra.agent.tier").map(String::as_str) == Some("balanced")
        }),
        "generic create_agent span should record tier=balanced when no explicit tier is provided and no reverse lookup match is available"
    );
}

#[test]
fn generic_child_invoke_agent_span_nests_under_parent_trace() {
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-generic-trace",
        "model": "parent-model",
        "choices": [{
            "message": { "role": "assistant", "content": "generic trace done" },
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

    let (_, spans, _) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            CapabilityToken::default(),
            default_budget(),
            Arc::new(factory),
        );
        let spawn = generic_spawn_config(
            "child-generic-trace",
            "parent-agent",
            "You are a trace-linked generic worker.",
            child_budget(32, 1, 0),
        );

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let parent_span = tracing::info_span!("parent_agent_turn");
                {
                    let _entered = parent_span.enter();
                    supervisor
                        .spawn_agent(spawn)
                        .expect("generic child should spawn under the parent trace");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            });
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "invoke_agent" && span.parent_name.as_deref() == Some("parent_agent_turn")
        }),
        "generic child invoke_agent span should be parented to the active parent trace"
    );
}

#[tokio::test]
async fn generic_spawn_without_tier_reverse_looks_up_parent_model_for_resolved_tier() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let mut tiers = TierMap::default();
    tiers.insert(
        "reasoning".to_string(),
        "parent-reasoning-model".to_string(),
    );
    tiers.insert("balanced".to_string(), "other-model".to_string());
    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec![],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers,
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-reasoning-model".into(),
    };

    let call_future = tool.call(
        serde_json::json!({
            "system_prompt": "You are a tier-labeled generic worker.",
            "task": "do something",
            "budget": {
                "max_tokens": 10,
                "max_turns": 1,
                "max_cost": "1",
                "max_sub_agents": 0
            }
        }),
        &CapabilityToken::default(),
    );
    let receive_future = async move {
        let message = receiver
            .recv()
            .await
            .expect("spawn tool should send one supervisor message");
        match message.payload {
            SupervisorPayload::Spawn(config, result_tx) => {
                let captured = (*config).clone();
                result_tx
                    .send(Ok(SpawnAck {
                        child_id: captured.agent_id.clone(),
                        agent_type: "generic".into(),
                    }))
                    .expect("spawn tool should still be awaiting the spawn acknowledgement");
                captured
            }
            other => panic!("expected SupervisorPayload::Spawn, got {other:?}"),
        }
    };

    let (result, captured) = tokio::join!(call_future, receive_future);
    result.expect("generic spawn should complete");

    assert_eq!(
        captured.agent_type, None,
        "this assertion must exercise generic mode, not configured mode"
    );
    assert_eq!(
        captured.tier, None,
        "the LLM did not request an explicit tier"
    );
    assert_eq!(
        captured.resolved_tier.as_deref(),
        Some("reasoning"),
        "generic spawn without tier should label the child with the first tier whose model matches the parent model"
    );
}
