#[tokio::test]
async fn wasm_module_fetch_writes_journal_entry_per_call() {
    let _guard = test_guard().await;

    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal_arc = Arc::new(RecordingJournal::default());
    let journal: Arc<dyn JournalStorage> = journal_arc.clone();
    let hook: Arc<dyn HookModule> = Arc::new(CapturingHook {
        captured: Arc::new(Mutex::new(Vec::new())),
    });

    let module = build_module(&server, hook, journal);

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    manager
        .call_tool(
            "github",
            "fetch",
            json!({ "url": server.url("/data") }),
            &capability("github"),
        )
        .await
        .expect("call_tool should succeed");

    // Golden Rule: an HttpRequest journal entry was written for the
    // fetch driven by the WASM module — not by the host helper.
    let entries = journal_arc.entries();
    assert!(
        entries
            .iter()
            .any(|e| matches!(e.entry, JournalEntryKind::HttpRequest { .. })),
        "WASM-driven fetch must write an HttpRequest journal entry"
    );
}

#[tokio::test]
async fn wasm_module_fetch_journal_entry_carries_configured_agent_id() {
    let _guard = test_guard().await;

    // Spec §Journal: every fetch entry must be attributed to the agent
    // that drove it. The module is configured with a specific AgentId;
    // entries written by `wasm_mcp_fetch` (via the `simulacra:mcp/http`
    // host import) must carry that AgentId so per-agent replay/audit
    // can read them back via `JournalStorage::read_all(agent_id)`.
    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal_arc = Arc::new(RecordingJournal::default());
    let journal: Arc<dyn JournalStorage> = journal_arc.clone();
    let hook: Arc<dyn HookModule> = Arc::new(CapturingHook {
        captured: Arc::new(Mutex::new(Vec::new())),
    });

    let agent_id = AgentId("agent-007".into());

    let module_file = fetcher_module_path();
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::HttpRequest, hook);
    let module = load_wasm_mcp_module(module_file.path())
        .expect("fetcher-mcp should load")
        .with_network_allowlist(vec![server.host_port()])
        .with_hooks(Arc::new(pipeline))
        .with_journal(Arc::clone(&journal))
        .with_agent_id(agent_id.clone());

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    manager
        .call_tool(
            "github",
            "fetch",
            json!({ "url": server.url("/data") }),
            &capability("github"),
        )
        .await
        .expect("call_tool should succeed");

    // `read_all(agent_id)` only returns entries attributed to this
    // agent. If `wasm_mcp_fetch` had stamped an empty AgentId the
    // entry would be filtered out and this assertion would fail.
    let attributed = journal_arc
        .read_all(&agent_id)
        .expect("read_all should succeed");
    assert!(
        attributed
            .iter()
            .any(|e| matches!(e.entry, JournalEntryKind::HttpRequest { .. })),
        "fetch journal entry should be attributed to the configured agent_id, got entries: {attributed:?}"
    );
}

#[tokio::test]
async fn shared_mcp_manager_attributes_each_fetch_to_calling_agent() {
    let _guard = test_guard().await;

    // Per-agent journal attribution (server mode): one shared
    // `McpManager` + one shared `WasmMcpModule` is reused across many
    // agents. Each agent's outbound `simulacra:mcp/http.fetch` journal
    // entry must carry that agent's `AgentId`, not the module's
    // construction-time bake-in.
    //
    // This is the property `simulacra-server` needs: a single per-process
    // MCP manager (so HTTP/SSE connection pools, cached components,
    // and capability checks are shared) but per-agent audit on the
    // way out. Today's `with_agent_id` baker-in is preserved as a
    // back-compat default for the CLI single-agent path; this test
    // proves the per-call override wins when present.
    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal_arc = Arc::new(RecordingJournal::default());
    let journal: Arc<dyn JournalStorage> = journal_arc.clone();
    let hook: Arc<dyn HookModule> = Arc::new(CapturingHook {
        captured: Arc::new(Mutex::new(Vec::new())),
    });

    // The module's bake-in agent_id is "default-cli-agent" (the
    // single-agent process value). Per-call attribution must override
    // it for each of the calling agents below.
    let module_file = fetcher_module_path();
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::HttpRequest, hook);
    let module = load_wasm_mcp_module(module_file.path())
        .expect("fetcher-mcp should load")
        .with_network_allowlist(vec![server.host_port()])
        .with_hooks(Arc::new(pipeline))
        .with_journal(Arc::clone(&journal))
        .with_agent_id(AgentId("default-cli-agent".into()));

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    let alice = AgentId("alice".into());
    let bob = AgentId("bob".into());

    manager
        .call_tool_for_agent(
            &alice,
            "github",
            "fetch",
            json!({ "url": server.url("/alice") }),
            &capability("github"),
        )
        .await
        .expect("alice's call_tool should succeed");
    manager
        .call_tool_for_agent(
            &bob,
            "github",
            "fetch",
            json!({ "url": server.url("/bob") }),
            &capability("github"),
        )
        .await
        .expect("bob's call_tool should succeed");

    let alice_entries = journal_arc
        .read_all(&alice)
        .expect("read_all alice should succeed");
    let bob_entries = journal_arc
        .read_all(&bob)
        .expect("read_all bob should succeed");
    let cli_default = AgentId("default-cli-agent".into());
    let default_entries = journal_arc
        .read_all(&cli_default)
        .expect("read_all default should succeed");

    let alice_http: Vec<_> = alice_entries
        .iter()
        .filter_map(|e| match &e.entry {
            JournalEntryKind::HttpRequest { url, .. } => Some(url.clone()),
            _ => None,
        })
        .collect();
    let bob_http: Vec<_> = bob_entries
        .iter()
        .filter_map(|e| match &e.entry {
            JournalEntryKind::HttpRequest { url, .. } => Some(url.clone()),
            _ => None,
        })
        .collect();
    let default_http: Vec<_> = default_entries
        .iter()
        .filter_map(|e| match &e.entry {
            JournalEntryKind::HttpRequest { url, .. } => Some(url.clone()),
            _ => None,
        })
        .collect();

    assert!(
        alice_http.iter().any(|u| u.ends_with("/alice")),
        "alice's fetch must be journaled under her agent_id, got: alice={alice_http:?} bob={bob_http:?} default={default_http:?}"
    );
    assert!(
        bob_http.iter().any(|u| u.ends_with("/bob")),
        "bob's fetch must be journaled under his agent_id, got: alice={alice_http:?} bob={bob_http:?} default={default_http:?}"
    );
    assert!(
        !alice_http.iter().any(|u| u.ends_with("/bob")),
        "alice must not see bob's fetch — per-agent attribution is broken"
    );
    assert!(
        !bob_http.iter().any(|u| u.ends_with("/alice")),
        "bob must not see alice's fetch — per-agent attribution is broken"
    );
    assert!(
        default_http.is_empty(),
        "the module's bake-in agent_id default must NOT receive entries when the per-call agent_id is non-empty, got: {default_http:?}"
    );
}

