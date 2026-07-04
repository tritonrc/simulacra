mod common;
#[allow(unused_imports)]
use common::*;

#[cfg(feature = "python")]
#[test]
fn shell_exec_python_c_flag_executes_inline_code_without_vfs_read() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, false, true), Arc::clone(&journal));

    let result = harness
        .cell
        .execute_shell(r#"python -c "print('inline python')""#)
        .expect("python -c should execute inline Monty code");

    assert_eq!(result.exit_code, 0);
    assert!(
        result.stdout.contains("inline python"),
        "expected python stdout, got {:?}",
        result.stdout
    );
    assert!(
        !journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, .. } if tool_name == "read_file"
            )
        }),
        "python -c inline code must not try to read a script from VFS"
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == r#"python -c "print('inline python')""# && *exit_code == 0
            )
        }),
        "expected python -c execution to append a ShellCommand journal entry"
    );
}

#[cfg(feature = "python")]
#[cfg(feature = "python")]
#[test]
fn shell_exec_python_participates_in_shell_pipelines_and_redirects() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, false, true), Arc::clone(&journal));

    let result = harness
        .cell
        .execute_shell(
            r#"python -c "print('alpha'); print('beta')" | grep beta > /workspace/out.txt"#,
        )
        .expect("python alias should participate in shell pipeline and redirect stages");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "");
    assert_eq!(harness.vfs.read("/workspace/out.txt").unwrap(), b"beta\n");
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::FileWrite { path, size_bytes }
                if path == "/workspace/out.txt" && *size_bytes == 5
        )),
        "expected final redirect to write through mediated VFS path"
    );
}

#[cfg(feature = "python")]
#[cfg(feature = "python")]
#[test]
fn shell_exec_python_dash_reads_script_from_pipeline_stdin() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, false, true), Arc::clone(&journal));

    let result = harness
        .cell
        .execute_shell(r#"echo "print('from stdin')" | python -"#)
        .expect("python - should execute script piped on stdin");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "from stdin\n");
    assert!(
        !journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, .. } if tool_name == "read_file"
            )
        }),
        "python - must execute stdin without trying to read a script path"
    );
}

#[cfg(feature = "python")]
#[cfg(feature = "python")]
#[test]
fn shell_exec_python_alias_uses_mediated_external_functions() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, false, true), Arc::clone(&journal));

    let result = harness
        .cell
        .execute_shell(
            r#"python -c "write_file('/workspace/out.txt', 'created'); print(read_file('/workspace/out.txt'))""#,
        )
        .expect("python alias should use the mediated Monty dispatcher");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "created\n");
    assert_eq!(
        harness.vfs.read("/workspace/out.txt").unwrap(),
        b"created",
        "write_file bridge should write through AgentCell mediation"
    );
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::FileWrite { path, size_bytes }
                if path == "/workspace/out.txt" && *size_bytes == 7
        )),
        "expected mediated Python write to produce FileWrite journal entry"
    );
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::ToolResult { tool_name, is_error, .. }
                if tool_name == "read_file" && !is_error
        )),
        "expected mediated Python read to produce read_file ToolResult journal entry"
    );
}

#[cfg(feature = "python")]
#[test]
fn shell_exec_python_execution_is_journaled_and_reads_script_through_mediated_path() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, false, true), Arc::clone(&journal));
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write("/workspace/script.py", b"print('hello from python')")
        .expect("seed script");

    let result = harness
        .cell
        .execute_shell("python3 /workspace/script.py")
        .expect("python alias should succeed");

    assert_eq!(result.exit_code, 0);
    assert!(
        result.stdout.contains("hello from python"),
        "expected python stdout, got {:?}",
        result.stdout
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, is_error, .. }
                    if tool_name == "read_file" && !is_error
            )
        }),
        "expected python alias to read the script through the mediated read_file path"
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "python3 /workspace/script.py" && *exit_code == 0
            )
        }),
        "expected python alias execution to append a ShellCommand journal entry"
    );
}

#[cfg(feature = "python")]
#[cfg(feature = "python")]
#[test]
fn shell_exec_python_script_read_is_mediated_by_paths_read_capability() {
    let journal = Arc::new(FakeJournalStorage::default());
    let capability = CapabilityToken {
        shell: true,
        python: true,
        paths_read: vec![],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..Default::default()
    };
    let harness = MemoryHarness::new(capability, Arc::clone(&journal));
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write("/workspace/script.py", b"print('blocked')")
        .expect("seed script");

    let result = harness
        .cell
        .execute_shell("python /workspace/script.py")
        .expect("python alias should surface script read denials as command results");

    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.contains("capability denied"),
        "expected mediated read denial in stderr, got {:?}",
        result.stderr
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "python /workspace/script.py" && *exit_code == 1
            )
        }),
        "expected failed python alias to append a ShellCommand journal entry"
    );
}
