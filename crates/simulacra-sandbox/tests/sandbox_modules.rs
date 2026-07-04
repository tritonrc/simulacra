mod common;
#[allow(unused_imports)]
use common::*;

#[test]
fn remote_module_stub_uses_parsed_url_authority_for_network_capability_checks() {
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:allowed.example.com"], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    let stub_url = "https://allowed.example.com@evil.example.com/entry.js";
    harness
        .cell
        .register_module_stub(stub_url, "export default 42;");

    let error = harness
        .execute_js(&format!(
            r#"
            import value from "{stub_url}";
            value;
            "#
        ))
        .expect_err("module fetch must be denied based on the actual URL host")
        .to_string();

    assert!(
        error.contains("capability denied") && error.contains("evil.example.com"),
        "module fetch capability checks must use the parsed URL authority, got {error}"
    );
}

#[test]
fn remote_module_fetch_with_denied_network_capability_fails_with_a_capability_error_message_surfaced_as_a_js_module_loading_error()
 {
    let harness = Harness::new(
        capability_with_network(&[], &[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_js(
            r#"
            import denied from "https://denied.invalid/pkg.js";
            denied;
            "#,
        )
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("denied.invalid") && error.contains("capability"),
        "expected remote module capability denial to surface in the JS module-loading error, got {error}"
    );
}

#[test]
fn fetch_http_to_an_allowed_host_returns_the_http_response_with_status_headers_and_body() {
    let server = spawn_http_server(
        200,
        &[("content-type", "text/plain"), ("x-test", "sandbox")],
        b"hello over http",
    );
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let response = harness
        .cell
        .fetch_http(&server.url("/allowed"), "GET", &[], None, None)
        .expect("allowed hosts should return a structured HTTP response");

    assert_eq!(response.status, 200);
    assert!(
        response
            .headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("x-test") && value == "sandbox"),
        "expected response headers to be preserved"
    );
    assert_eq!(response.body, b"hello over http".to_vec());
}

#[test]
fn fetch_http_to_a_denied_host_returns_sandboxerror_capability_denied() {
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"blocked");
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:api.github.com"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .cell
        .fetch_http(&server.url("/blocked"), "GET", &[], None, None)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("capability denied") && error.contains("127.0.0.1"),
        "expected denied host fetch to return CapabilityDenied, got {error}"
    );
}

#[test]
fn fetch_http_uses_parsed_url_authority_for_network_capability_checks() {
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:allowed.example.com"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    let url = "https://allowed.example.com:443@evil.example.com/secret";

    let error = harness
        .cell
        .fetch_http(url, "GET", &[], None, None)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("capability denied") && error.contains("evil.example.com"),
        "network capability checks must use the parsed URL authority, got {error}"
    );
}

#[test]
fn fetch_http_network_error_returns_http_with_the_url_and_the_failure_reason() {
    // Use port 1 — a privileged port that is almost never listening,
    // avoiding the TOCTOU race of bind-then-drop-then-connect.
    let url = "http://127.0.0.1:1/offline".to_string();
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .cell
        .fetch_http(&url, "GET", &[], None, None)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains(&url),
        "expected network failure to include the URL, got {error}"
    );
    // The error message varies by platform (e.g. "Connection refused" on Linux,
    // "connection refused" on macOS). Just verify the request failed with an Http error.
    assert!(
        matches!(
            harness
                .cell
                .fetch_http(&url, "GET", &[], None, None)
                .unwrap_err(),
            simulacra_sandbox::SandboxError::Http(_)
        ),
        "expected SandboxError::Http for a network failure"
    );
}

#[test]
fn fetch_http_produces_a_sandbox_http_fetch_span_with_url_method_and_status() {
    let server = spawn_http_server(202, &[("content-type", "text/plain")], b"accepted");
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    let url = server.url("/span");

    let (_result, spans, _events) = capture_operation(|| {
        harness
            .cell
            .fetch_http(&url, "POST", &[], Some(b"payload"), None)
    });

    assert!(
        spans.iter().any(|span| {
            span.fields.get("simulacra.operation.name") == Some(&"sandbox_http_fetch".to_string())
                && span.fields.get("simulacra.http.url") == Some(&url)
                && span.fields.get("simulacra.http.method") == Some(&"POST".to_string())
                && span.fields.get("simulacra.http.status") == Some(&"202".to_string())
        }),
        "expected sandbox_http_fetch span with simulacra.http.url, simulacra.http.method, and simulacra.http.status"
    );
}

// ---------------------------------------------------------------------------
// SB1: read_file journaling coverage
// ---------------------------------------------------------------------------

#[test]
fn read_file_with_valid_capability_writes_a_toolresult_journal_entry() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/hello.txt", b"hello");

    harness
        .read_file("/workspace/hello.txt")
        .expect("read should succeed with valid capability");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult {
                    tool_name,
                    content,
                    is_error,
                    ..
                } if tool_name == "read_file"
                    && content.contains("5 bytes")
                    && content.contains("/workspace/hello.txt")
                    && !is_error
            )
        }),
        "expected a ToolResult journal entry for read_file with path and byte count, got {entries:?}"
    );
}

// ---------------------------------------------------------------------------
// GSB4: read_file journal entry kind is ToolResult
// ---------------------------------------------------------------------------

#[test]
fn read_file_journal_entry_kind_is_tool_result_not_file_write_or_code_execution() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness
        .vfs
        .seed_file("/workspace/data.bin", b"\x00\x01\x02");

    harness
        .read_file("/workspace/data.bin")
        .expect("read should succeed");

    let entries = journal.entries();
    let read_entries: Vec<_> = entries
        .iter()
        .filter(|e| {
            matches!(
                &e.entry,
                JournalEntryKind::ToolResult { tool_name, .. } if tool_name == "read_file"
            )
        })
        .collect();
    assert_eq!(
        read_entries.len(),
        1,
        "expected exactly one ToolResult journal entry for read_file, got {read_entries:?}"
    );
    // Ensure no FileWrite or CodeExecution entries were written
    assert!(
        !entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::FileWrite { .. } | JournalEntryKind::CodeExecution { .. }
        )),
        "read_file must not produce FileWrite or CodeExecution journal entries"
    );
}

// ---------------------------------------------------------------------------
// SB2/GSB2: read_file budget enforcement
// ---------------------------------------------------------------------------

#[test]
fn read_file_with_budget_exhausted_returns_budget_exhausted_error() {
    let journal = Arc::new(FakeJournalStorage::default());
    // Exhaust the turns budget so check_budget() fails
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        budget_with_overrides(1, 1, 0, 0),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/hello.txt", b"hello");

    let error = harness.read_file("/workspace/hello.txt").unwrap_err();

    assert_budget_exhausted(error, &["turns"], "1", "1");
    // VFS should not have been touched
    harness.vfs.clear_observations();
}

#[test]
fn read_file_with_vfs_bytes_budget_exhausted_returns_budget_exhausted_error() {
    let journal = Arc::new(FakeJournalStorage::default());
    // Exhaust the vfs_bytes budget
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        budget_with_overrides(0, 0, 100, 100),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/hello.txt", b"hello");

    let error = harness.read_file("/workspace/hello.txt").unwrap_err();

    assert_budget_exhausted(error, &["vfs_bytes"], "100", "100");
}

// ---------------------------------------------------------------------------
// SB3: Capability denial does NOT consume budget
// ---------------------------------------------------------------------------

#[test]
fn capability_denial_on_read_file_does_not_increment_used_turns() {
    let budget = budget_with_overrides(10, 0, 0, 0);
    let harness = Harness::new(
        capability(&[], &[], false, false), // no read capability
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/secret.txt", b"secret");
    let before = budget_counter(&budget, "used_turns");

    let _ = harness.read_file("/workspace/secret.txt");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after, before,
        "capability denial on read_file must not increment used_turns"
    );
}

#[test]
fn capability_denial_on_write_file_does_not_increment_used_turns() {
    let budget = budget_with_overrides(10, 0, 0, 0);
    let harness = Harness::new(
        capability(&[], &[], false, false), // no write capability
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    let _ = harness.write_file("/workspace/denied.txt", b"denied");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after, before,
        "capability denial on write_file must not increment used_turns"
    );
}
