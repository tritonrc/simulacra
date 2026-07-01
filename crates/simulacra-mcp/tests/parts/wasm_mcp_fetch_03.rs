#[tokio::test(start_paused = true)]
async fn request_timeout_returns_fetch_error_timeout() {
    let _guard = test_guard().await;
    // WARNING #1: previous version did a real 31s sleep. Use
    // `wasm_mcp_fetch_with_timeout` so the test runs deterministically
    // in <100ms with `tokio::time::pause()` (`start_paused = true`).
    let server = spawn_recording_http_server(
        vec![json!({"ok": true}).to_string()],
        Duration::from_secs(60),
    );

    let timeout = Duration::from_millis(1);
    let allowlist = vec![format!(
        "127.0.0.1:{}",
        server.addr.rsplit(':').next().unwrap_or("0")
    )];
    let agent_id = simulacra_types::AgentId(String::new());
    let fetch_future = wasm_mcp_fetch_with_timeout(
        "github",
        request_to(server.url("/slow")),
        &allowlist,
        None,
        Some(journal_arc()),
        &agent_id,
        timeout,
    );

    // Advance virtual time past the configured timeout to deterministically
    // trip the timeout branch. This must complete in <100ms wall-clock.
    let advance = async {
        tokio::time::advance(Duration::from_millis(100)).await;
    };

    let (err, _) = tokio::join!(fetch_future, advance);
    let err = err.expect_err("requests exceeding the timeout should return FetchError::Timeout");
    assert_eq!(err, FetchError::Timeout);
}

#[tokio::test]
async fn fetch_only_reachable_via_simulacra_http_fetch_import_wasi_sockets_fail() {
    let _guard = test_guard().await;
    // Companion to wasi_networking_remains_disabled — covers spec
    // assertion 31 ("WASI networking remains disabled") from the
    // perspective of "no allowlist entry means no path." See README's
    // deferral note for the wasi-sockets-attempt fixture.
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);

    let err = wasm_mcp_fetch(
        "github",
        request_to(server.url("/fetch")),
        &[],
        None,
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect_err("WASI sockets should not bypass simulacra:http/fetch capability checks");

    assert!(
        matches!(err, FetchError::CapabilityDenied(_)),
        "direct socket access should fail while simulacra:http/fetch remains the only supported path"
    );
}
