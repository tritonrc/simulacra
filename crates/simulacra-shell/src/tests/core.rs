use super::*;

#[test]
fn echo_hello_writes_stdout_and_returns_zero() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello");

    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn echo_hello_pipe_grep_hello_returns_match_and_zero() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello | grep hello");

    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn echo_hello_pipe_grep_world_returns_empty_stdout_and_one() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello | grep world");

    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 1);
}

#[test]
fn redirect_truncate_then_cat_reads_back_written_contents() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        HashMap::new(),
        "echo hello > /file.txt && cat /file.txt",
    );

    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
    assert_eq!(vfs.read("/file.txt").unwrap(), b"hello\n");
}

#[test]
fn redirect_append_accumulates_lines_in_order() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        HashMap::new(),
        "echo a >> /f.txt && echo b >> /f.txt && cat /f.txt",
    );

    assert_eq!(result.stdout, "a\nb\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
    assert_eq!(vfs.read("/f.txt").unwrap(), b"a\nb\n");
}

#[test]
fn ls_root_on_empty_vfs_lists_nothing_and_returns_zero() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "ls /");

    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn ls_root_after_creating_files_lists_them_sorted() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    run_shell(vfs, HashMap::new(), "echo x > /c.txt");
    run_shell(vfs, HashMap::new(), "echo x > /a.txt");
    run_shell(vfs, HashMap::new(), "echo x > /b.txt");

    let result = run_shell(vfs, HashMap::new(), "ls /");

    assert_eq!(result.stdout, "a.txt\nb.txt\nc.txt\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn unknown_command_returns_127_and_command_not_found_stderr() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "nonexistent_cmd");

    assert_eq!(result.stdout, "");
    assert_eq!(result.exit_code, 127);
    assert!(
        result.stderr.contains("command not found: nonexistent_cmd"),
        "stderr should mention command-not-found, got {:?}",
        result.stderr
    );
}

#[test]
fn false_and_echo_yes_short_circuits_without_output() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "false && echo yes");

    assert_eq!(result.stdout, "");
    assert_eq!(result.exit_code, 1);
}

#[test]
fn false_or_echo_fallback_executes_right_hand_side() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "false || echo fallback");

    assert_eq!(result.stdout, "fallback\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn shell_command_execution_emits_span_with_command_and_exit_code() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| run_shell(vfs, HashMap::new(), "echo hello"));

    let span = spans
        .iter()
        .find(|span| field_matches(span, "simulacra.operation.name", "shell_command"))
        .unwrap_or_else(|| panic!("expected shell_command span, got {spans:#?}"));

    assert!(field_matches(span, "simulacra.shell.command", "echo"));
    assert!(field_matches(span, "simulacra.shell.argc", "1"));
    assert!(field_matches(span, "simulacra.shell.exit_code", "0"));
}

#[test]
fn simulacra_shell_commands_counter_increments_per_command_execution() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| run_shell(vfs, HashMap::new(), "echo left && echo right"));

    let command_spans = shell_command_spans(&spans);

    assert_eq!(
        command_spans.len(),
        2,
        "expected one shell_command emission per executed command so simulacra.shell.commands can increment per execution; got {spans:#?}"
    );
    assert!(command_spans.iter().all(|span| field_matches(
        span,
        "simulacra.shell.command",
        "echo"
    )));
    assert!(
        command_spans
            .iter()
            .all(|span| field_matches(span, "simulacra.shell.argc", "1"))
    );
}

#[test]
fn pipe_chains_emit_parent_span_with_child_stage_spans() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| run_shell(vfs, HashMap::new(), "echo hello | grep hello"));

    let command_spans = shell_command_spans(&spans);
    assert_eq!(
        command_spans.len(),
        2,
        "expected a shell_command span for each pipeline stage, got {spans:#?}"
    );

    let parent_name = command_spans[0]
        .parent
        .as_deref()
        .unwrap_or_else(|| panic!("expected pipeline parent span, got {spans:#?}"));

    assert!(
        spans.iter().any(|span| span.name == parent_name),
        "expected captured parent span named {parent_name}, got {spans:#?}"
    );

    for span in command_spans {
        assert_eq!(
            span.parent.as_deref(),
            Some(parent_name),
            "all pipeline stage spans should share the same parent; got {spans:#?}"
        );
    }
}

// ---------------------------------------------------------------------------
// SH1: ${VAR} brace-style expansion
// ---------------------------------------------------------------------------

#[test]
fn brace_style_variable_expansion_replaces_with_env_value() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let mut env = HashMap::new();
    env.insert("HOME".to_string(), "/home/simulacra".to_string());

    let result = run_shell(vfs, env, "echo ${HOME}");

    assert_eq!(result.stdout, "/home/simulacra\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn brace_style_expansion_adjacent_to_text() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let mut env = HashMap::new();
    env.insert("USER".to_string(), "simulacra".to_string());

    let result = run_shell(vfs, env, "echo ${USER}_home");

    assert_eq!(result.stdout, "simulacra_home\n");
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// SH4: Redirect failure paths
// ---------------------------------------------------------------------------

#[test]
fn redirect_to_root_directory_reports_error() {
    // Redirecting to "/" should fail because "/" is a directory, not a file.
    // The executor must report the error: non-zero exit code, stderr message,
    // and stdout preserved (not cleared on failure).
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello > /");

    // stdout is preserved because the redirect failed
    assert_eq!(result.stdout, "hello\n");
    // Exit code reflects the redirect failure
    assert_ne!(result.exit_code, 0);
    // stderr contains the redirect error
    assert!(
        result.stderr.contains("redirect"),
        "expected redirect error in stderr, got: {}",
        result.stderr
    );
}

#[test]
fn redirect_failure_span_records_final_exit_code() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| run_shell(vfs, HashMap::new(), "echo hello > /"));

    let span = shell_command_spans(&spans)
        .into_iter()
        .find(|span| field_matches(span, "simulacra.shell.command", "echo"))
        .unwrap_or_else(|| panic!("expected echo command span, got {spans:#?}"));

    assert!(
        field_matches(span, "simulacra.shell.exit_code", "1"),
        "redirect failure should record final exit code 1, got {span:#?}"
    );
}

// ---------------------------------------------------------------------------
// SH5: Parser edge case tests
// ---------------------------------------------------------------------------
