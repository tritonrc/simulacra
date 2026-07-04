mod common;
#[allow(unused_imports)]
use common::*;

#[test]
fn capability_denial_on_execute_shell_does_not_increment_used_turns() {
    let budget = budget_with_overrides(10, 0, 0, 0);
    let harness = Harness::new(
        capability(&[], &[], false, false), // shell = false
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    let _ = harness.execute_shell("echo denied");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after, before,
        "capability denial on execute_shell must not increment used_turns"
    );
}

#[test]
fn capability_denial_on_execute_js_does_not_increment_used_turns() {
    let budget = budget_with_overrides(10, 0, 0, 0);
    let harness = Harness::new(
        capability(&[], &[], false, false), // javascript = false
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    let _ = harness.execute_js("1 + 1");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after, before,
        "capability denial on execute_js must not increment used_turns"
    );
}

#[test]
fn capability_denial_on_fetch_http_does_not_increment_used_turns() {
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"ok");
    let budget = budget_with_overrides(10, 0, 0, 0);
    let harness = Harness::new(
        capability_with_network(&[], &[], &[], false, false), // no network capability
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    let _ = harness
        .cell
        .fetch_http(&server.url("/denied"), "GET", &[], None, None);

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after, before,
        "capability denial on fetch_http must not increment used_turns"
    );
}

// ---------------------------------------------------------------------------
// GSB1: list_dir on a denied path returns CapabilityDenied
// ---------------------------------------------------------------------------

#[test]
fn list_dir_on_path_outside_paths_read_returns_capability_denied() {
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/secrets/key.pem", b"secret");

    let error = harness.list_dir("/secrets").unwrap_err();

    assert!(
        matches!(
            error,
            ExpectedSandboxError::CapabilityDenied(CapabilityDenied { ref operation, .. })
                if operation == "read_file"
        ),
        "expected CapabilityDenied for list_dir on denied path, got {error:?}"
    );
}

#[test]
fn list_dir_on_denied_path_does_not_touch_vfs() {
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/secrets/key.pem", b"secret");
    harness.vfs.clear_observations();

    let _ = harness.list_dir("/secrets");

    assert_eq!(
        harness.vfs.list_count(),
        0,
        "denied list_dir must not hit the VFS"
    );
}

// ---------------------------------------------------------------------------
// GSB5: journal write ordering relative to VFS execution for read_file
// ---------------------------------------------------------------------------

#[test]
fn read_file_journal_entry_is_written_after_successful_vfs_read() {
    // Verify that a successful read_file produces a journal entry
    // (the journal entry contains the byte count, which means VFS read
    // must have completed before the journal write).
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness
        .vfs
        .seed_file("/workspace/ordered.txt", b"ordered content");

    let data = harness
        .read_file("/workspace/ordered.txt")
        .expect("read should succeed");

    assert_eq!(data, b"ordered content");
    let entries = journal.entries();
    let tool_result = entries
        .iter()
        .find(|e| {
            matches!(
                &e.entry,
                JournalEntryKind::ToolResult { tool_name, .. } if tool_name == "read_file"
            )
        })
        .expect("expected a ToolResult journal entry after read_file");
    // The journal entry should contain the correct byte count, proving
    // the VFS read completed before the journal write.
    match &tool_result.entry {
        JournalEntryKind::ToolResult { content, .. } => {
            assert!(
                content.contains("15 bytes"),
                "journal entry should reflect the actual bytes read (15), got: {content}"
            );
        }
        _ => unreachable!(),
    }
}

#[test]
fn read_file_on_missing_path_writes_an_error_journal_entry() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    let _ = harness.read_file("/workspace/missing.txt");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::ToolResult { tool_name, is_error, content, .. }
                if tool_name == "read_file" && *is_error && content.contains("missing.txt")
        )),
        "a failed read_file should produce an error journal entry, got {entries:?}"
    );
}

// ---------------------------------------------------------------------------
// GSB12: fs.readFileSync/fs.writeFileSync from JS — success path
// ---------------------------------------------------------------------------

#[test]
fn fs_readfilesync_from_js_on_allowed_path_returns_file_content() {
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness
        .vfs
        .seed_file("/workspace/greeting.txt", b"hello from vfs");

    let output = harness
        .execute_js("fs.readFileSync('/workspace/greeting.txt')")
        .expect("fs.readFileSync on an allowed path should succeed");

    assert_eq!(
        output.result.as_deref(),
        Some("hello from vfs"),
        "fs.readFileSync should return the file content as a string"
    );
}

#[test]
fn fs_readfilesync_from_js_on_allowed_path_produces_journal_entry() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, true),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness
        .vfs
        .seed_file("/workspace/journaled.txt", b"journal me");

    harness
        .execute_js("fs.readFileSync('/workspace/journaled.txt')")
        .expect("fs.readFileSync on an allowed path should succeed");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::ToolResult { tool_name, is_error, .. }
                if tool_name == "read_file" && !is_error
        )),
        "fs.readFileSync from JS should produce a ToolResult journal entry via the FsProxy, got {entries:?}"
    );
}

#[test]
fn fs_writefilesync_from_js_on_allowed_path_writes_to_vfs_and_produces_journal_entry() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &["/output/**"], false, true),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    harness
        .execute_js("fs.writeFileSync('/output/result.txt', 'written from js')")
        .expect("fs.writeFileSync on an allowed path should succeed");

    // Verify VFS state
    assert!(
        harness.vfs.exists("/output/result.txt"),
        "fs.writeFileSync should write to the VFS"
    );
    let data = harness.vfs.inner.read("/output/result.txt").unwrap();
    assert_eq!(
        String::from_utf8_lossy(&data),
        "written from js",
        "fs.writeFileSync should write the correct content"
    );

    // Verify journal entry
    let entries = journal.entries();
    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::FileWrite { path, .. }
                if path == "/output/result.txt"
        )),
        "fs.writeFileSync from JS should produce a FileWrite journal entry via the FsProxy, got {entries:?}"
    );
}

#[test]
fn js_fs_extended_operations_are_journaled_and_spanned_through_fs_proxy() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &["/workspace/**"], false, true),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/old.txt", b"move me");
    harness.vfs.seed_file("/workspace/delete.txt", b"delete me");

    let (result, spans, _) = capture_operation(|| {
        harness.execute_js(
            r#"
            fs.mkdirSync('/workspace/newdir');
            fs.readdirSync('/workspace');
            const stat = fs.statSync('/workspace/old.txt');
            if (!stat.isFile || stat.size !== 7) throw new Error('bad stat');
            fs.renameSync('/workspace/old.txt', '/workspace/newdir/new.txt');
            fs.unlinkSync('/workspace/delete.txt');
            'done';
            "#,
        )
    });

    let output = result.expect("extended JS fs operations should succeed through FsProxy");
    assert_eq!(output.result.as_deref(), Some("done"));

    for expected in [
        "sandbox_fs_proxy_mkdir",
        "sandbox_fs_proxy_list_dir",
        "sandbox_fs_proxy_stat",
        "sandbox_fs_proxy_rename",
        "sandbox_fs_proxy_remove",
    ] {
        assert!(
            spans.iter().any(|span| span
                .fields
                .get("simulacra.operation.name")
                .is_some_and(|operation| operation == expected)),
            "expected span {expected}, got {spans:?}"
        );
    }

    let entries = journal.entries();
    for expected in ["mkdir", "list_dir", "stat", "rename", "remove"] {
        assert!(
            entries.iter().any(|entry| matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, is_error, .. }
                    if tool_name == expected && !is_error
            )),
            "expected successful {expected} ToolResult journal entry, got {entries:?}"
        );
    }
}

#[test]
fn js_append_file_sync_requires_only_write_capability() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &["/workspace/**"], false, true),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/log.txt", b"first");

    let output = harness
        .execute_js(
            r#"
            fs.appendFileSync('/workspace/log.txt', ' second');
            fs.appendFileSync('/workspace/new.txt', 'created');
            'done';
            "#,
        )
        .expect("appendFileSync should be a mediated write operation");

    assert_eq!(output.result.as_deref(), Some("done"));
    assert_eq!(
        harness.vfs.read("/workspace/log.txt").unwrap(),
        b"first second"
    );
    assert_eq!(harness.vfs.read("/workspace/new.txt").unwrap(), b"created");
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::FileWrite { path, size_bytes }
                if path == "/workspace/log.txt" && *size_bytes == 7
        )),
        "expected appendFileSync to journal appended byte count as FileWrite"
    );
}

#[test]
fn js_rename_sync_moves_directories_with_write_capability_on_both_roots() {
    let harness = Harness::new(
        capability(&[], &["/workspace/from", "/workspace/to"], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.mkdir("/workspace/from").unwrap();
    harness.vfs.seed_file("/workspace/from/a.txt", b"a");
    harness.vfs.mkdir("/workspace/from/sub").unwrap();
    harness.vfs.seed_file("/workspace/from/sub/b.txt", b"b");

    harness
        .execute_js("fs.renameSync('/workspace/from', '/workspace/to');")
        .expect("renameSync should move directories through the mediated host operation");

    assert!(!harness.vfs.exists("/workspace/from"));
    assert_eq!(harness.vfs.read("/workspace/to/a.txt").unwrap(), b"a");
    assert_eq!(harness.vfs.read("/workspace/to/sub/b.txt").unwrap(), b"b");
}
