#[tokio::test]
async fn wasm_module_fetch_dispatches_through_host_pipeline() {
    let _guard = test_guard().await;

    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal: Arc<dyn JournalStorage> = Arc::new(RecordingJournal::default());
    let captured: Arc<Mutex<Vec<(Operation, Phase, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let hook: Arc<dyn HookModule> = Arc::new(CapturingHook {
        captured: Arc::clone(&captured),
    });

    let module = build_module(&server, hook, Arc::clone(&journal));

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    let result = manager
        .call_tool(
            "github",
            "fetch",
            json!({ "url": server.url("/data") }),
            &capability("github"),
        )
        .await
        .expect("call_tool should succeed end-to-end");

    let status = result
        .get("status")
        .and_then(Value::as_u64)
        .expect("status");
    assert_eq!(
        status, 200,
        "WASM module should observe HTTP 200 from host fetch"
    );

    // Real HTTP server actually saw the WASM module's request.
    let server_requests = server.requests();
    assert_eq!(
        server_requests.len(),
        1,
        "host fetch should reach the recording server exactly once"
    );
    assert!(
        server_requests[0].contains("GET /data"),
        "WASM module's GET /data should land on the recording server, got: {:?}",
        server_requests[0]
    );

    // Hook pipeline ran for both Before and After phases through the
    // real `simulacra_hooks` machinery (not a parallel test trait).
    let captured_events = captured.lock().expect("capture mutex").clone();
    assert!(
        captured_events
            .iter()
            .any(|(op, phase, _)| *op == Operation::HttpRequest && *phase == Phase::Before),
        "Phase::Before HttpRequest hook should fire for WASM-driven fetch"
    );
    assert!(
        captured_events
            .iter()
            .any(|(op, phase, _)| *op == Operation::HttpRequest && *phase == Phase::After),
        "Phase::After HttpRequest hook should fire for WASM-driven fetch"
    );
}

#[tokio::test]
async fn wasm_module_fetch_to_unallowed_host_returns_capability_denied() {
    let _guard = test_guard().await;

    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal: Arc<dyn JournalStorage> = Arc::new(RecordingJournal::default());
    let hook: Arc<dyn HookModule> = Arc::new(CapturingHook {
        captured: Arc::new(Mutex::new(Vec::new())),
    });

    // Allowlist only an unrelated host so the WASM module's fetch is
    // denied at the host's `check_network_allowlist` gate.
    let module_file = fetcher_module_path();
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::HttpRequest, hook);
    let module = load_wasm_mcp_module(module_file.path())
        .expect("fetcher-mcp should load")
        .with_network_allowlist(vec!["api.github.com:443".into()])
        .with_hooks(Arc::new(pipeline))
        .with_journal(Arc::clone(&journal));

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    let err = manager
        .call_tool(
            "github",
            "fetch",
            json!({ "url": server.url("/data") }),
            &capability("github"),
        )
        .await
        .expect_err("denied fetch should surface as execution failure inside WASM module");

    let msg = format!("{err:?}");
    assert!(
        msg.to_lowercase().contains("capability_denied")
            || msg.to_lowercase().contains("capability denied"),
        "denied fetch should surface FetchError::CapabilityDenied, got: {msg}"
    );

    // The real HTTP server should never have been contacted.
    assert!(
        server.requests().is_empty(),
        "denied fetch must NOT reach the network"
    );
}

#[tokio::test]
async fn wasm_module_fetch_blocked_by_before_phase_hook_returns_hook_denied() {
    let _guard = test_guard().await;

    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal: Arc<dyn JournalStorage> = Arc::new(RecordingJournal::default());

    let module_file = fetcher_module_path();
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::HttpRequest, Arc::new(DenyingHook));
    let module = load_wasm_mcp_module(module_file.path())
        .expect("fetcher-mcp should load")
        .with_network_allowlist(vec![server.host_port()])
        .with_hooks(Arc::new(pipeline))
        .with_journal(Arc::clone(&journal));

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    let err = manager
        .call_tool(
            "github",
            "fetch",
            json!({ "url": server.url("/data") }),
            &capability("github"),
        )
        .await
        .expect_err("hook-denied fetch should surface as execution failure");

    let msg = format!("{err:?}");
    assert!(
        msg.to_lowercase().contains("hook_denied") || msg.to_lowercase().contains("hook denied"),
        "hook-denied fetch should surface FetchError::HookDenied, got: {msg}"
    );

    // Hook denied before dispatch — recording server never saw a request.
    assert!(
        server.requests().is_empty(),
        "hook-denied fetch must NOT reach the network"
    );
}

#[tokio::test]
async fn wasm_module_fetch_request_redaction_reaches_remote_server() {
    let _guard = test_guard().await;

    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal: Arc<dyn JournalStorage> = Arc::new(RecordingJournal::default());

    let module_file = fetcher_module_path();
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::HttpRequest, Arc::new(RedactingHook));
    let module = load_wasm_mcp_module(module_file.path())
        .expect("fetcher-mcp should load")
        .with_network_allowlist(vec![server.host_port()])
        .with_hooks(Arc::new(pipeline))
        .with_journal(Arc::clone(&journal));

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    let mut last_err = None;
    for _ in 0..3 {
        match manager
            .call_tool(
                "github",
                "fetch",
                json!({ "url": server.url("/data") }),
                &capability("github"),
            )
            .await
        {
            Ok(_) => {
                last_err = None;
                break;
            }
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    if let Some(err) = last_err {
        panic!("redacted fetch should succeed: {err:?}");
    }

    let server_requests = server.requests();
    assert_eq!(server_requests.len(), 1);
    let req = &server_requests[0];
    assert!(
        req.to_lowercase().contains("authorization: redacted"),
        "before-phase redaction of `authorization: secret -> redacted` should land on remote server, got: {req:?}"
    );
    assert!(
        !req.to_lowercase().contains("authorization: secret"),
        "original `authorization: secret` must NOT reach the network, got: {req:?}"
    );
}

