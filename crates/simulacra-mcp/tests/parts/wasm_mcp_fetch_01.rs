#[test]
fn fetch_to_host_outside_network_allowlist_returns_capability_denied() {
    let allowed = vec!["api.github.com:443".to_string()];

    assert!(
        !check_network_allowlist("example.com:443", &allowed),
        "hosts outside the allowlist should be denied before any fetch dispatch"
    );
}

#[test]
fn fetch_to_host_with_wildcard_port_permits_any_port() {
    let allowed = vec!["api.github.com:*".to_string()];

    assert!(
        check_network_allowlist("api.github.com:8443", &allowed),
        "host:* allowlist entries should permit any destination port for that host"
    );
}

#[test]
fn fetch_to_subdomain_glob_permits_subdomain_at_listed_port() {
    let allowed = vec!["*.example.com:443".to_string()];

    assert!(
        check_network_allowlist("api.example.com:443", &allowed),
        "subdomain glob entries should match subdomains at the configured port"
    );
}

#[test]
fn empty_network_allowlist_rejects_all_outbound_http() {
    assert!(
        !check_network_allowlist("api.github.com:443", &[]),
        "an empty network allowlist should reject all outbound HTTP"
    );
}

#[tokio::test]
async fn allowlist_denial_through_wasm_mcp_fetch_returns_capability_denied() {
    let _guard = test_guard().await;
    // WARNING #2: previous allowlist coverage exercised only the pure
    // `check_network_allowlist` helper. This test wires the same allowlist
    // through the full `wasm_mcp_fetch` path and asserts the wired return
    // surface matches `FetchError::CapabilityDenied` from spec Assertion 22.
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);

    let err = wasm_mcp_fetch(
        "github",
        request_to(server.url("/blocked")),
        // Allowlist that explicitly does NOT include the test server's port.
        &["api.github.com:443".to_string()],
        None,
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect_err("denied allowlist should be returned through the fetch entrypoint");

    assert!(
        matches!(err, FetchError::CapabilityDenied(_)),
        "wired denial should surface as FetchError::CapabilityDenied, got {err:?}"
    );
    assert_eq!(
        server.request_count(),
        0,
        "denied allowlist must short-circuit before hitting the wire"
    );
}

#[tokio::test]
async fn operation_http_request_before_hook_is_invoked_before_wire_dispatch() {
    let _guard = test_guard().await;
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);
    let hooks = RecordingHookPipeline::new(HookAction::Continue, HookAction::Continue);

    let response = wasm_mcp_fetch(
        "github",
        request_to(server.url("/fetch")),
        &[format!(
            "127.0.0.1:{}",
            server.addr.rsplit(':').next().unwrap_or("0")
        )],
        Some(hooks.pipeline()),
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect("fetch should succeed");

    assert_eq!(response.status, 200);
    assert_eq!(server.request_count(), 1);
    assert!(
        hooks
            .captured()
            .iter()
            .any(|(operation, phase, _)| *operation == Operation::HttpRequest
                && *phase == Phase::Before),
        "before-phase HTTP hooks should run before the wire dispatch"
    );
}

#[tokio::test]
async fn phase_before_deny_verdict_returns_hook_denied_to_module() {
    let _guard = test_guard().await;
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);
    let hooks =
        RecordingHookPipeline::new(HookAction::Deny("blocked by policy"), HookAction::Continue);

    let err = wasm_mcp_fetch(
        "github",
        request_to(server.url("/fetch")),
        &[format!(
            "127.0.0.1:{}",
            server.addr.rsplit(':').next().unwrap_or("0")
        )],
        Some(hooks.pipeline()),
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect_err("before-phase deny verdicts should be returned to the module");

    assert_eq!(err, FetchError::HookDenied("blocked by policy".to_string()));
}

#[tokio::test]
async fn phase_before_redact_modifies_request_headers_before_dispatch() {
    let _guard = test_guard().await;
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);
    let hooks = RecordingHookPipeline::new(
        HookAction::RedactRequestHeader("authorization", "secret", "redacted"),
        HookAction::Continue,
    );

    wasm_mcp_fetch(
        "github",
        request_to(server.url("/fetch")),
        &[format!(
            "127.0.0.1:{}",
            server.addr.rsplit(':').next().unwrap_or("0")
        )],
        Some(hooks.pipeline()),
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect("fetch should succeed after request redaction");

    assert!(
        server
            .requests()
            .iter()
            .any(|request| request.contains("redacted")),
        "before-phase redaction should mutate outbound request headers before dispatch"
    );
}

#[tokio::test]
async fn phase_before_url_rewrite_is_rechecked_against_allowlist() {
    let _guard = test_guard().await;
    let allowed_server =
        spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);
    let blocked_server =
        spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);
    let hooks = RecordingHookPipeline::new(
        HookAction::RewriteRequestUrl(blocked_server.url("/blocked")),
        HookAction::Continue,
    );

    let err = wasm_mcp_fetch(
        "github",
        request_to(allowed_server.url("/fetch")),
        &[format!(
            "127.0.0.1:{}",
            allowed_server.addr.rsplit(':').next().unwrap_or("0")
        )],
        Some(hooks.pipeline()),
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect_err("hook-rewritten egress must be rechecked against the allowlist");

    assert!(
        matches!(err, FetchError::CapabilityDenied(_)),
        "rewritten URL outside the allowlist should be denied, got {err:?}"
    );
    assert_eq!(
        allowed_server.request_count(),
        0,
        "the original allowed destination should not be contacted after rewrite"
    );
    assert_eq!(
        blocked_server.request_count(),
        0,
        "the rewritten unallowed destination must not be contacted"
    );
}
