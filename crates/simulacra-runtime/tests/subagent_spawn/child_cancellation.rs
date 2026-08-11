#[test]
fn agent_task_factory_attaches_child_cancellation_token() {
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-cancelled",
        "model": "child-model",
        "choices": [{
            "message": { "role": "assistant", "content": "should not be requested" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
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
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: None,
        acp_child_runtime: None,
    };
    let token = CancellationToken::new(Duration::from_secs(1));
    token.signal();

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
        .block_on(factory.create_task(
            spawn_config(
                "child-cancelled",
                "parent-agent",
                child_budget(32, 1, 0),
            ),
            token,
        ))
        .expect("child task should return a terminal cancellation output");

    assert_eq!(output.exit_reason, ExitReason::Cancelled);
    assert!(
        server.requests.lock().unwrap().is_empty(),
        "cancelled child should exit before sending a provider request"
    );
}
