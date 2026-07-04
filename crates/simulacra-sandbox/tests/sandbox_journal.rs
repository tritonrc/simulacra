mod common;
#[allow(unused_imports)]
use common::*;

#[test]
fn execute_shell_redirect_write_is_mediated_by_paths_write_capability() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], true, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness.vfs.clear_observations();

    let result = harness
        .execute_shell("echo blocked > /workspace/blocked.txt")
        .expect("shell should surface mediated write denial as command result");

    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.contains("permission denied") || result.stderr.contains("capability denied"),
        "expected mediated write denial, got {:?}",
        result.stderr
    );
    assert_eq!(
        harness.vfs.write_count(),
        0,
        "capability denial must happen before the shell touches VFS write"
    );
    assert!(!harness.vfs.exists("/workspace/blocked.txt"));
}

#[test]
fn execute_js_writes_a_codeexecution_journal_entry_with_language_javascript() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    let _ = harness.execute_js("1 + 1");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::CodeExecution { language } if language == "javascript"
            )
        }),
        "expected a CodeExecution journal entry for JavaScript execution"
    );
}

#[test]
fn capability_denial_writes_a_journal_entry_recording_the_denied_operation_and_reason() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    let _ = harness.write_file("/workspace/denied.txt", b"denied");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            let payload = journal_payload(entry);
            payload.contains("write_file") && payload.contains("denied")
        }),
        "expected a journal entry recording the denied operation and reason"
    );
}

#[test]
fn budget_exhaustion_writes_a_journal_entry_recording_the_exhausted_resource() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &[], true, false),
        budget_with_overrides(1, 1, 0, 0),
        Arc::clone(&journal),
    );

    let _ = harness.execute_shell("echo blocked");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            let payload = journal_payload(entry);
            payload.contains("tool_calls") || payload.contains("turns")
        }),
        "expected a journal entry recording the exhausted budget resource"
    );
}

#[test]
fn execute_shell_increments_used_tool_calls_by_one() {
    let budget = unlimited_budget();
    let harness = Harness::new(
        capability(&[], &[], true, false),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    harness
        .execute_shell("echo hello")
        .expect("shell command should succeed");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after,
        before + 1,
        "execute_shell must consume one tool-call unit"
    );
}

#[test]
fn execute_js_increments_used_tool_calls_by_one() {
    let budget = unlimited_budget();
    let harness = Harness::new(
        capability(&[], &[], false, true),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    let _ = harness.execute_js("1 + 1");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after,
        before + 1,
        "execute_js must consume one tool-call unit"
    );
}

#[test]
fn write_file_increments_used_vfs_bytes_by_the_written_byte_count() {
    let budget = budget_with_overrides(0, 0, 1024, 0);
    let harness = Harness::new(
        capability(&[], &["/output/result.txt"], false, false),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_vfs_bytes");

    harness
        .write_file("/output/result.txt", b"hello")
        .expect("write should succeed");

    let after = budget_counter(&budget, "used_vfs_bytes");
    assert_eq!(after, before + 5, "write_file must consume VFS byte budget");
}

#[test]
fn zero_budget_limits_are_treated_as_unlimited() {
    let budget = budget_with_overrides(0, 99, 0, 2048);
    let harness = Harness::new(
        capability(&[], &["/output/result.txt"], true, false),
        budget,
        Arc::new(FakeJournalStorage::default()),
    );

    harness
        .execute_shell("echo still-allowed")
        .expect("tool-call budget limit 0 should be unlimited");
    harness
        .write_file("/output/result.txt", b"still allowed")
        .expect("vfs-bytes budget limit 0 should be unlimited");
}

#[test]
fn workspace_read_glob_allows_reading_a_workspace_file() {
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/foo.txt", b"workspace");

    let data = harness
        .read_file("/workspace/foo.txt")
        .expect("workspace glob should allow reading files under /workspace");

    assert_eq!(data, b"workspace");
}

#[test]
fn workspace_read_glob_denies_reading_a_secret_outside_workspace() {
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/secrets/key.pem", b"secret");

    let error = harness.read_file("/secrets/key.pem").unwrap_err();

    assert!(matches!(error, ExpectedSandboxError::CapabilityDenied(_)));
}

#[test]
fn output_write_glob_allows_writing_under_output() {
    let harness = Harness::new(
        capability(&[], &["/output/**"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    harness
        .write_file("/output/result.txt", b"ok")
        .expect("output glob should allow writing under /output");

    assert!(harness.vfs.exists("/output/result.txt"));
}

#[test]
fn output_write_glob_denies_writing_outside_output() {
    let harness = Harness::new(
        capability(&[], &["/output/**"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .write_file("/workspace/sneaky.txt", b"nope")
        .unwrap_err();

    assert!(matches!(error, ExpectedSandboxError::CapabilityDenied(_)));
    assert!(!harness.vfs.exists("/workspace/sneaky.txt"));
}

#[test]
fn wildcard_root_read_pattern_allows_reading_any_path() {
    let harness = Harness::new(
        capability(&["/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/secrets/key.pem", b"secret");

    let data = harness
        .read_file("/secrets/key.pem")
        .expect("root wildcard should allow reading any path");

    assert_eq!(data, b"secret");
}

#[test]
fn empty_path_capabilities_deny_all_reads_and_writes() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/foo.txt", b"read me");

    let read_error = harness.read_file("/workspace/foo.txt").unwrap_err();
    let write_error = harness
        .write_file("/workspace/bar.txt", b"write me")
        .unwrap_err();

    assert!(matches!(
        read_error,
        ExpectedSandboxError::CapabilityDenied(_)
    ));
    assert!(matches!(
        write_error,
        ExpectedSandboxError::CapabilityDenied(_)
    ));
}

// ---------------------------------------------------------------------------
// SB8: Path-capability security edges (traversal, normalization)
// ---------------------------------------------------------------------------

#[test]
fn path_traversal_starting_outside_allowed_prefix_is_denied() {
    // A path that uses .. to escape from the root — it never starts with /workspace
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/etc/passwd", b"root:x:0:0");

    let error = harness.read_file("/etc/passwd").unwrap_err();

    assert!(
        matches!(error, ExpectedSandboxError::CapabilityDenied(_)),
        "reading a path outside the allowed prefix must be denied, got {error:?}"
    );
}

#[test]
fn relative_path_without_allowed_prefix_is_denied() {
    // Relative paths that don't start with the allowed absolute prefix are denied
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness.read_file("../secret/data.txt").unwrap_err();

    assert!(
        matches!(error, ExpectedSandboxError::CapabilityDenied(_)),
        "relative path traversal must be denied, got {error:?}"
    );
}

#[test]
fn write_to_path_outside_allowed_prefix_is_denied() {
    let harness = Harness::new(
        capability(&[], &["/output/**"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .write_file("/etc/crontab", b"malicious")
        .unwrap_err();

    assert!(
        matches!(error, ExpectedSandboxError::CapabilityDenied(_)),
        "writing outside the allowed prefix must be denied, got {error:?}"
    );
    assert!(
        !harness.vfs.exists("/etc/crontab"),
        "denied write must not create files outside the allowed path"
    );
}

#[test]
fn exact_path_capability_does_not_allow_sibling_paths() {
    let harness = Harness::new(
        capability(&["/workspace/allowed.txt"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/secret.txt", b"secret");

    let error = harness.read_file("/workspace/secret.txt").unwrap_err();

    assert!(
        matches!(error, ExpectedSandboxError::CapabilityDenied(_)),
        "exact path capability must not allow sibling files, got {error:?}"
    );
}
