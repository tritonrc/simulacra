mod common;
#[allow(unused_imports)]
use common::*;

#[test]
fn shell_exec_node_script_executes_through_quickjs_and_returns_output() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, true, false), journal);
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write(
        "/workspace/script.js",
        br#"
        console.log("hello from node");
        42;
        "#,
    )
    .expect("seed script");

    let result = harness
        .cell
        .execute_shell("node /workspace/script.js")
        .expect("node alias should execute the script");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stderr, "");
    assert_eq!(result.stdout, "hello from node\n42\n");
}

#[test]
fn shell_exec_node_without_arguments_returns_usage_error_with_exit_code_one() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, true, false), journal);

    let result = harness
        .cell
        .execute_shell("node")
        .expect("node without arguments should return a usage error result");

    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "Usage: node <script.js>\n");
}

#[test]
fn shell_exec_node_eval_flag_executes_inline_code_without_vfs_read() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, true, false), Arc::clone(&journal));

    let result = harness
        .cell
        .execute_shell(r#"node -e "console.log('inline node'); 21 + 21""#)
        .expect("node -e should execute inline QuickJS code");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stderr, "");
    assert_eq!(result.stdout, "inline node\n42\n");
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::CodeExecution { language } if language == "javascript"
            )
        }),
        "expected node -e to execute through the JS path"
    );
    assert!(
        !journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, .. } if tool_name == "read_file"
            )
        }),
        "node -e inline code must not try to read a script from VFS"
    );
}

#[test]
fn shell_exec_node_participates_in_shell_pipelines() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, true, false), journal);
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write(
        "/workspace/script.js",
        b"console.log('alpha'); console.log('beta');",
    )
    .expect("seed script");

    let result = harness
        .cell
        .execute_shell("node /workspace/script.js | grep beta")
        .expect("node alias should run as a shell pipeline stage");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "beta\n");
    assert_eq!(result.stderr, "");
}

#[test]
fn shell_exec_node_dash_reads_script_from_pipeline_stdin() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, true, false), Arc::clone(&journal));

    let result = harness
        .cell
        .execute_shell(r#"echo "console.log('from stdin')" | node -"#)
        .expect("node - should execute script piped on stdin");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "from stdin\n");
    assert!(
        !journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, .. } if tool_name == "read_file"
            )
        }),
        "node - must execute stdin without trying to read a script path"
    );
}

#[test]
fn shell_exec_node_output_redirect_uses_mediated_shell_vfs_write() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, true, false), Arc::clone(&journal));

    let result = harness
        .cell
        .execute_shell(r#"node -e "console.log('to file')" > /workspace/out.txt"#)
        .expect("node alias output should be redirectable");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "");
    assert_eq!(
        harness.vfs.read("/workspace/out.txt").unwrap(),
        b"to file\n"
    );
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::FileWrite { path, size_bytes }
                if path == "/workspace/out.txt" && *size_bytes == 8
        )),
        "expected shell redirect to write through mediated VFS path"
    );
}

#[test]
fn shell_exec_nodejs_alias_matches_node_execution() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, true, false), journal);
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write(
        "/workspace/script.js",
        br#"
        console.log("hello from alias");
        "done";
        "#,
    )
    .expect("seed script");

    let node = harness
        .cell
        .execute_shell("node /workspace/script.js")
        .expect("node alias should succeed");
    let nodejs = harness
        .cell
        .execute_shell("nodejs /workspace/script.js")
        .expect("nodejs alias should succeed");

    assert_eq!(nodejs.exit_code, node.exit_code);
    assert_eq!(nodejs.stdout, node.stdout);
    assert_eq!(nodejs.stderr, node.stderr);
}

#[test]
fn shell_exec_node_requires_javascript_capability() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, false, false), journal);
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write("/workspace/script.js", b"console.log('blocked');")
        .expect("seed script");

    let result = harness
        .cell
        .execute_shell("node /workspace/script.js")
        .expect("node alias should surface JS capability denials as command results");

    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.contains("capability denied"),
        "expected JS capability denial in stderr, got {:?}",
        result.stderr
    );
}

#[test]
fn shell_exec_node_execution_is_journaled() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = MemoryHarness::new(capability_token(true, true, false), Arc::clone(&journal));
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write("/workspace/script.js", b"console.log('journaled');")
        .expect("seed script");

    let result = harness
        .cell
        .execute_shell("node /workspace/script.js")
        .expect("node alias should succeed");

    assert_eq!(result.exit_code, 0);
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::CodeExecution { language } if language == "javascript"
            )
        }),
        "expected node alias execution to append a JavaScript CodeExecution journal entry"
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, is_error, .. }
                    if tool_name == "read_file" && !is_error
            )
        }),
        "expected node alias to read the script through the mediated read_file path"
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "node /workspace/script.js" && *exit_code == 0
            )
        }),
        "expected node alias execution to append a ShellCommand journal entry"
    );
}

#[test]
fn shell_exec_node_script_read_is_mediated_by_paths_read_capability() {
    let journal = Arc::new(FakeJournalStorage::default());
    let capability = CapabilityToken {
        shell: true,
        javascript: true,
        paths_read: vec![],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..Default::default()
    };
    let harness = MemoryHarness::new(capability, Arc::clone(&journal));
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write("/workspace/script.js", b"console.log('blocked');")
        .expect("seed script");

    let result = harness
        .cell
        .execute_shell("node /workspace/script.js")
        .expect("node alias should surface script read denials as command results");

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
                    if command == "node /workspace/script.js" && *exit_code == 1
            )
        }),
        "expected failed node alias to append a ShellCommand journal entry"
    );
}

#[test]
fn node_shell_alias_produces_the_same_operation_spans_as_execute_js() {
    let shell_journal = Arc::new(FakeJournalStorage::default());
    let js_journal = Arc::new(FakeJournalStorage::default());
    let shell_harness = MemoryHarness::new(capability_token(true, true, false), shell_journal);
    let js_harness = MemoryHarness::new(capability_token(true, true, false), js_journal);
    let fs: &dyn VirtualFs = shell_harness.vfs.as_ref();
    fs.write("/workspace/script.js", b"1 + 1")
        .expect("seed script");

    let (_, shell_spans) = capture_spans(|| {
        shell_harness
            .cell
            .execute_shell("node /workspace/script.js")
            .unwrap()
    });
    let (_, js_spans) = capture_spans(|| js_harness.cell.execute_js("1 + 1").unwrap());

    // The node alias goes through execute_shell and mediated read_file, so it
    // has those extra spans compared to direct execute_js.
    // But it must include the same JS execution spans.
    let js_ops = span_operations(&js_spans);
    let shell_ops = span_operations(&shell_spans);
    for op in &js_ops {
        assert!(
            shell_ops.contains(op),
            "node alias should include JS span '{op}', got: {shell_ops:?}"
        );
    }
    assert!(
        shell_ops.contains(&"sandbox_shell_exec".to_string()),
        "node alias should include sandbox_shell_exec span"
    );
    assert!(
        shell_ops.contains(&"sandbox_read_file".to_string()),
        "node alias should include sandbox_read_file span from reading the script file"
    );
}
