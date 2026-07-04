mod common;
#[allow(unused_imports)]
use common::*;

#[test]
fn agent_cell_new_accepts_vfs_capability_budget_and_journal() {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let capability = CapabilityToken::default();
    let budget = unlimited_budget();
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());

    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let _cell = AgentCell::new(vfs, capability, budget, journal, http_client);
}

#[test]
fn agent_cell_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<AgentCell>();
}

#[test]
fn two_agent_cells_with_different_vfs_references_do_not_share_filesystem_state() {
    let left = Harness::new(
        capability(&[], &["/workspace/shared.txt"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    let right = Harness::new(
        capability(&[], &["/workspace/shared.txt"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    left.write_file("/workspace/shared.txt", b"left")
        .expect("left write should succeed");

    assert!(left.vfs.exists("/workspace/shared.txt"));
    assert!(
        !right.vfs.exists("/workspace/shared.txt"),
        "separate cells must not share VFS state"
    );
}

#[test]
fn agent_cell_holds_a_persistent_shellexecutor_so_shell_state_survives_across_execute_shell_calls()
{
    let harness = Harness::new(
        capability(&[], &[], true, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let first = harness
        .execute_shell("export GREETING=hello-from-shell")
        .expect("persistent shell executor should accept exporting state");
    assert_eq!(
        first.exit_code, 0,
        "export should succeed so shell state can persist across calls"
    );

    let second = harness
        .execute_shell("echo $GREETING")
        .expect("second shell call should observe the exported environment");
    assert_eq!(
        second.stdout.trim(),
        "hello-from-shell",
        "shell environment variables should survive across execute_shell calls"
    );
}

#[test]
fn agent_cell_js_exec_does_not_leak_global_state_across_calls() {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    harness
        .execute_js("globalThis.__simulacra_counter = 41; Object.prototype.polluted = true;")
        .expect("first JS execution should run");

    let output = harness
        .execute_js(
            r#"
            [
              typeof globalThis.__simulacra_counter,
              Object.prototype.polluted === true
            ].join("|")
            "#,
        )
        .expect("second JS execution should run in a fresh context");
    assert_eq!(
        output.result.as_deref(),
        Some("undefined|false"),
        "JS globals must not survive across execute_js calls within the same AgentCell"
    );
}

#[test]
fn vfs_errors_propagate_as_sandbox_vfs_errors() {
    let harness = Harness::new(
        capability(&["/missing.txt"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness.read_file("/missing.txt").unwrap_err();

    assert!(matches!(
        error,
        ExpectedSandboxError::Vfs(VfsError::NotFound(path)) if path == "/missing.txt"
    ));
}

#[test]
fn shell_command_not_found_returns_a_shell_result_not_a_sandbox_error() {
    let harness = Harness::new(
        capability(&[], &[], true, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let result = harness
        .execute_shell("nonexistent_cmd")
        .expect("shell errors should surface as CommandResult");

    assert_ne!(
        result.exit_code, 0,
        "a missing command must produce a non-zero exit code"
    );
    assert!(
        !result.stderr.is_empty(),
        "a missing command must produce some diagnostic on stderr"
    );
}

#[test]
fn journal_write_failure_does_not_prevent_execution_and_is_logged_at_error() {
    let journal = Arc::new(FakeJournalStorage::default());
    journal.fail_next_append();
    let harness = Harness::new(
        capability(&[], &["/output/result.txt"], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    let (result, _spans, events) =
        capture_operation(|| harness.write_file("/output/result.txt", b"ok"));

    result.expect("the VFS write should still succeed when the journal backend fails");
    assert!(harness.vfs.exists("/output/result.txt"));
    assert!(
        events.iter().any(|event| {
            event.level == "ERROR"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("journal") || value.contains("append"))
        }),
        "expected an ERROR log for the journal append failure"
    );
}

#[test]
fn read_file_produces_a_sandbox_read_file_span_with_vfs_path() {
    let harness = Harness::new(
        capability(&["/workspace/foo.txt"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/foo.txt", b"span me");

    let (_result, spans, _events) = capture_operation(|| harness.read_file("/workspace/foo.txt"));

    assert!(
        spans.iter().any(|span| {
            span.fields.get("simulacra.operation.name") == Some(&"sandbox_read_file".to_string())
                && span.fields.get("simulacra.vfs.path") == Some(&"/workspace/foo.txt".to_string())
        }),
        "expected sandbox_read_file span with simulacra.vfs.path"
    );
}

#[test]
fn write_file_produces_a_sandbox_write_file_span_with_path_and_bytes() {
    let harness = Harness::new(
        capability(&[], &["/output/result.txt"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let (_result, spans, _events) =
        capture_operation(|| harness.write_file("/output/result.txt", b"hello"));

    assert!(
        spans.iter().any(|span| {
            span.fields.get("simulacra.operation.name") == Some(&"sandbox_write_file".to_string())
                && span.fields.get("simulacra.vfs.path") == Some(&"/output/result.txt".to_string())
                && span.fields.get("simulacra.vfs.bytes") == Some(&"5".to_string())
        }),
        "expected sandbox_write_file span with simulacra.vfs.path and simulacra.vfs.bytes"
    );
}

#[test]
fn execute_shell_produces_a_sandbox_shell_exec_span_with_command() {
    let harness = Harness::new(
        capability(&[], &[], true, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let (_result, spans, _events) = capture_operation(|| harness.execute_shell("echo hello"));

    assert!(
        spans.iter().any(|span| {
            span.fields.get("simulacra.operation.name") == Some(&"sandbox_shell_exec".to_string())
                && span.fields.get("simulacra.shell.command") == Some(&"echo hello".to_string())
        }),
        "expected sandbox_shell_exec span with simulacra.shell.command"
    );
}

#[test]
fn execute_js_produces_a_sandbox_js_exec_span() {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let (_result, spans, _events) = capture_operation(|| harness.execute_js("1 + 1"));

    assert!(
        spans.iter().any(|span| {
            span.fields.get("simulacra.operation.name") == Some(&"sandbox_js_exec".to_string())
        }),
        "expected sandbox_js_exec span"
    );
}

#[test]
fn capability_denials_emit_warn_events_with_operation_and_reason_on_the_current_span() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let (_result, _spans, events) =
        capture_operation(|| harness.write_file("/workspace/denied.txt", b"denied"));

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event.current_span.is_some()
                && event.fields.get("simulacra.capability.operation")
                    == Some(&"write_file".to_string())
                && event
                    .fields
                    .get("simulacra.capability.reason")
                    .map(|value| value.contains("denied"))
                    .unwrap_or(false)
        }),
        "expected a WARN event on the current span for capability denial"
    );
}

#[test]
fn capability_denials_increment_counter_with_operation_labels_for_each_denial_type() {
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"denied");
    let harness = Harness::new(
        capability_with_network(&[], &[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/secret.txt", b"secret");

    let (_result, _spans, events) = capture_operation(|| {
        let _ = harness.write_file("/workspace/denied.txt", b"blocked");
        let _ = harness.read_file("/workspace/secret.txt");
        let _ = harness.execute_shell("echo blocked");
        let _ = harness.execute_js("1 + 1");
        let _ = harness
            .cell
            .fetch_http(&server.url("/blocked"), "GET", &[], None, None);
    });

    let denial_counter_events = events
        .iter()
        .filter(|event| event.fields.get("simulacra.capability.denials") == Some(&"1".to_string()))
        .collect::<Vec<_>>();

    assert_eq!(
        denial_counter_events.len(),
        5,
        "expected one simulacra.capability.denials counter increment per denied operation, got {events:#?}"
    );

    for expected_operation in [
        "paths_write",
        "paths_read",
        "shell",
        "javascript",
        "network",
    ] {
        assert!(
            denial_counter_events.iter().any(|event| {
                event.fields.get("operation") == Some(&expected_operation.to_string())
            }),
            "expected simulacra.capability.denials counter event labeled with operation={expected_operation}, got {events:#?}"
        );
    }
}

#[test]
fn budget_exhaustion_emits_warn_events_with_resource_used_and_limit_on_the_current_span() {
    let harness = Harness::new(
        capability(&[], &[], true, false),
        budget_with_overrides(1, 1, 0, 0),
        Arc::new(FakeJournalStorage::default()),
    );

    let (_result, _spans, events) = capture_operation(|| harness.execute_shell("echo blocked"));

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event.current_span.is_some()
                && event
                    .fields
                    .get("simulacra.budget.resource")
                    .map(|value| value == "tool_calls" || value == "turns")
                    .unwrap_or(false)
                && event.fields.get("simulacra.budget.used") == Some(&"1".to_string())
                && event.fields.get("simulacra.budget.limit") == Some(&"1".to_string())
        }),
        "expected a WARN event on the current span for budget exhaustion"
    );
}

#[test]
fn list_dir_checks_paths_read_budget_and_delegates_to_vfs() {
    let harness = Harness::new(
        capability(&["/workspace"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/a.txt", b"a");
    harness.vfs.seed_file("/workspace/b.txt", b"b");

    let entries = harness
        .list_dir("/workspace")
        .expect("list_dir should proxy to the VFS once implemented");

    assert_eq!(entries, vec!["a.txt".to_string(), "b.txt".to_string()]);
}

#[test]
fn fetch_http_with_denied_network_capability_returns_capability_denied_and_does_not_make_a_request()
{
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"ok");
    let harness = Harness::new(
        capability_with_network(&[], &[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .cell
        .fetch_http(&server.url("/denied"), "GET", &[], None, None)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("capability denied") && error.contains("127.0.0.1"),
        "expected denied network fetch error, got {error}"
    );
    assert_eq!(
        server.request_count(),
        0,
        "denied network fetches must not hit the HTTP client"
    );
}
