#[tokio::test]
async fn wasm_module_uses_configured_http_client_for_outbound_fetches() {
    let _guard = test_guard().await;

    // W4: a custom `reqwest::Client` installed via
    // `WasmMcpModule::with_http_client` must be the one that issues
    // outbound `simulacra:mcp/http.fetch` calls. We prove this by injecting
    // a client whose default `User-Agent` is set to a sentinel string,
    // then asserting the recording HTTP server saw it on the wire.
    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal: Arc<dyn JournalStorage> = Arc::new(RecordingJournal::default());
    let captured: Arc<Mutex<Vec<(Operation, Phase, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let hook: Arc<dyn HookModule> = Arc::new(CapturingHook {
        captured: Arc::clone(&captured),
    });

    let custom_client = reqwest::Client::builder()
        // The recording fixture echoes raw bytes from a single read — the
        // sentinel header (rather than `.user_agent(...)`, which on some
        // hyper paths splits the initial write differently) is the
        // safest knob for proving the configured client issues the
        // request.
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                "x-simulacra-w4-sentinel",
                reqwest::header::HeaderValue::from_static("present"),
            );
            headers
        })
        // Match the default-client constraints so the recording fixture
        // (single-read, HTTP/1.1) keeps working.
        .tcp_nodelay(false)
        .http1_only()
        .pool_max_idle_per_host(0)
        .build()
        .expect("custom client should build");

    let module = build_module(&server, hook, Arc::clone(&journal)).with_http_client(custom_client);

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
        .expect("call_tool should succeed with custom client");

    let requests = server.requests();
    assert!(
        !requests.is_empty(),
        "recording server should have observed the outbound request"
    );
    assert!(
        requests
            .iter()
            .any(|r| r.contains("x-simulacra-w4-sentinel")),
        "configured custom client must issue the wire request — sentinel header missing from {requests:?}"
    );
}
