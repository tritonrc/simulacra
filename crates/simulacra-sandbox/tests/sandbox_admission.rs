mod common;
#[allow(unused_imports)]
use common::*;

#[test]
fn read_file_with_denied_paths_read_returns_capability_denied_and_does_not_touch_vfs() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/foo.txt", b"secret");
    harness.vfs.clear_observations();

    let error = harness.read_file("/workspace/foo.txt").unwrap_err();

    assert!(matches!(
        error,
        ExpectedSandboxError::CapabilityDenied(CapabilityDenied { operation, .. }) if operation == "read_file"
    ));
    assert_eq!(
        harness.vfs.read_count(),
        0,
        "denied reads must not hit the VFS"
    );
}

#[test]
fn read_file_with_denied_paths_read_surfaces_operation_and_reason_to_agent() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/foo.txt", b"secret");

    let error = harness.read_file("/workspace/foo.txt").unwrap_err();

    match error {
        ExpectedSandboxError::CapabilityDenied(denied) => {
            assert_eq!(denied.operation, "read_file");
            assert_eq!(denied.reason, "read access denied for /workspace/foo.txt");
        }
        other => panic!("expected capability denial, got {other:?}"),
    }
}

#[test]
fn write_file_with_denied_paths_write_returns_capability_denied_and_does_not_write() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .write_file("/workspace/output.txt", b"hello")
        .unwrap_err();

    assert!(matches!(
        error,
        ExpectedSandboxError::CapabilityDenied(CapabilityDenied { operation, .. }) if operation == "write_file"
    ));
    assert_eq!(
        harness.vfs.write_count(),
        0,
        "denied writes must not hit the VFS"
    );
    assert!(!harness.vfs.exists("/workspace/output.txt"));
}

#[test]
fn execute_shell_with_shell_false_returns_capability_denied_and_does_not_execute() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_shell("echo blocked > /workspace/blocked.txt")
        .unwrap_err();

    assert!(matches!(error, ExpectedSandboxError::CapabilityDenied(_)));
    assert!(!harness.vfs.exists("/workspace/blocked.txt"));
}

#[test]
fn execute_js_with_javascript_false_returns_capability_denied_and_does_not_execute_js() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_js("fs.writeFileSync('/workspace/blocked.txt', 'x')")
        .unwrap_err();

    assert!(matches!(error, ExpectedSandboxError::CapabilityDenied(_)));
    assert!(!harness.vfs.exists("/workspace/blocked.txt"));
}

#[test]
fn execute_js_with_javascript_false_surfaces_operation_and_reason_to_agent() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_js("fs.writeFileSync('/workspace/blocked.txt', 'x')")
        .unwrap_err();

    match error {
        ExpectedSandboxError::CapabilityDenied(denied) => {
            assert_eq!(denied.operation, "javascript");
            assert_eq!(denied.reason, "javascript capability not granted");
        }
        other => panic!("expected capability denial, got {other:?}"),
    }
}

#[test]
fn write_file_when_vfs_bytes_budget_is_exhausted_returns_budget_exhausted_and_does_not_write() {
    let harness = Harness::new(
        capability(&[], &["/workspace/output.txt"], false, false),
        budget_with_overrides(0, 0, 1, 1),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .write_file("/workspace/output.txt", b"hello")
        .unwrap_err();

    assert_budget_exhausted(error, &["vfs_bytes"], "1", "1");
    assert!(!harness.vfs.exists("/workspace/output.txt"));
}

#[test]
fn write_file_that_would_exceed_vfs_bytes_budget_is_rejected_before_write() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &["/workspace/output.txt"], false, false),
        budget_with_overrides(0, 0, 1, 0),
        Arc::clone(&journal),
    );

    let error = harness
        .write_file("/workspace/output.txt", b"hello")
        .unwrap_err();

    assert_budget_exhausted(error, &["vfs_bytes"], "5", "1");
    assert!(!harness.vfs.exists("/workspace/output.txt"));
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::ToolResult { tool_name, is_error, .. }
                if tool_name == "vfs_bytes" && *is_error
        )),
        "expected boundary-crossing write to journal budget exhaustion"
    );
}

#[test]
fn concurrent_write_file_reserves_vfs_bytes_without_overshooting_limit() {
    let budget = budget_with_overrides(0, 0, 10, 0);
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let vfs: Arc<dyn VirtualFs> = Arc::new(SlowWriteFs::new(Duration::from_millis(50)));
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = Arc::new(AgentCell::new(
        vfs,
        capability(&[], &["/workspace/**"], false, false),
        Arc::clone(&budget),
        journal,
        http_client,
    ));
    let barrier = Arc::new(Barrier::new(3));

    let mut handles = Vec::new();
    for idx in 0..2 {
        let cell = Arc::clone(&cell);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            cell.write_file(&format!("/workspace/{idx}.txt"), b"123456")
        }));
    }

    barrier.wait();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("writer thread should not panic"))
        .collect::<Vec<_>>();

    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one 6-byte write should fit into a 10-byte VFS budget: {results:?}"
    );
    assert_eq!(
        results.iter().filter(|result| result.is_err()).count(),
        1,
        "one concurrent write must be rejected before overshooting the budget: {results:?}"
    );
    assert_eq!(budget_counter(&budget, "used_vfs_bytes"), 6);
}

#[test]
fn concurrent_execute_shell_reserves_turns_without_overshooting_limit() {
    let budget = budget_with_overrides(1, 0, 0, 0);
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let vfs: Arc<dyn VirtualFs> = Arc::new(SlowWriteFs::new(Duration::from_millis(50)));
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = Arc::new(AgentCell::new(
        vfs,
        capability(&[], &["/workspace/**"], true, false),
        Arc::clone(&budget),
        journal,
        http_client,
    ));
    let barrier = Arc::new(Barrier::new(3));

    let mut handles = Vec::new();
    for idx in 0..2 {
        let cell = Arc::clone(&cell);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            cell.execute_shell(&format!("echo hi > /workspace/{idx}.txt"))
        }));
    }

    barrier.wait();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("shell thread should not panic"))
        .collect::<Vec<_>>();

    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one shell command should reserve the single available turn: {results:?}"
    );
    assert_eq!(
        results.iter().filter(|result| result.is_err()).count(),
        1,
        "one concurrent shell command must be rejected before overshooting turns: {results:?}"
    );
    assert_eq!(budget_counter(&budget, "used_turns"), 1);
}

#[test]
fn execute_shell_when_tool_calls_budget_is_exhausted_returns_budget_exhausted_and_does_not_execute()
{
    let harness = Harness::new(
        capability(&[], &[], true, false),
        budget_with_overrides(1, 1, 0, 0),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_shell("echo blocked > /workspace/blocked.txt")
        .unwrap_err();

    assert_budget_exhausted(error, &["tool_calls", "turns"], "1", "1");
    assert!(!harness.vfs.exists("/workspace/blocked.txt"));
}

#[test]
fn execute_js_when_tool_calls_budget_is_exhausted_returns_budget_exhausted_and_does_not_execute() {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        budget_with_overrides(1, 1, 0, 0),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness.execute_js("1 + 1").unwrap_err();

    assert_budget_exhausted(error, &["tool_calls", "turns"], "1", "1");
}

#[test]
fn execute_js_respects_configured_script_executor_permit() {
    let executor = ScriptExecutor::new(1);
    let _held_permit = executor
        .try_acquire_permit()
        .expect("test should reserve the only script permit");
    let vfs = Arc::new(SpyFs::new());
    let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
    let budget = unlimited_budget();
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let mut cell = AgentCell::new(
        vfs_dyn,
        capability(&[], &[], false, true),
        budget,
        journal,
        http_client,
    );
    cell.set_script_executor(executor.clone());

    let error = cell.execute_js("1 + 1").unwrap_err();

    match sandbox_error_to_expected(error) {
        ExpectedSandboxError::Internal(message) => assert!(
            message.contains("script executor permit unavailable"),
            "unexpected internal error: {message}"
        ),
        other => panic!("expected script executor internal error, got {other:?}"),
    }
}

#[test]
fn write_file_writes_a_filewrite_journal_entry_with_path_and_size_before_returning() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &["/output/result.txt"], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    harness
        .write_file("/output/result.txt", b"abc")
        .expect("write should succeed once the proxy exists");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::FileWrite { path, size_bytes }
                    if path == "/output/result.txt" && *size_bytes == 3
            )
        }),
        "expected a FileWrite journal entry for the proxied VFS write"
    );
}

#[test]
fn execute_shell_writes_a_shellcommand_journal_entry_with_command_and_exit_code() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &[], true, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    let result = harness
        .execute_shell("echo hello")
        .expect("shell command should succeed once proxied");

    assert_eq!(result.exit_code, 0);
    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "echo hello" && *exit_code == 0
            )
        }),
        "expected a ShellCommand journal entry with command and exit code"
    );
}

#[test]
fn execute_shell_cat_read_is_mediated_by_paths_read_capability() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &["/workspace/**"], true, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/secret.txt", b"secret");
    harness.vfs.clear_observations();

    let result = harness
        .execute_shell("cat /workspace/secret.txt")
        .expect("shell should surface mediated read denial as command result");

    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.contains("permission denied") || result.stderr.contains("capability denied"),
        "expected mediated read denial, got {:?}",
        result.stderr
    );
    assert_eq!(
        harness.vfs.read_count(),
        0,
        "capability denial must happen before the shell touches VFS read"
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "cat /workspace/secret.txt" && *exit_code != 0
            )
        }),
        "expected denied shell command to still be journaled"
    );
}
