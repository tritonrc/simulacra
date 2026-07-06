use super::*;

// ---------------------------------------------------------------------------
// Pipeline / list semantics — verifying && / ; / || drive a list correctly
// ---------------------------------------------------------------------------

#[test]
fn and_runs_rhs_when_lhs_succeeds() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "true && echo ran");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "ran\n");
}

#[test]
fn and_does_not_run_rhs_when_lhs_fails() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "false && echo should-not-run");

    assert_ne!(result.exit_code, 0, "exit code must be nonzero on failure");
    assert!(
        !result.stdout.contains("should-not-run"),
        "rhs of && must not run when lhs fails, got stdout={:?}",
        result.stdout
    );
}

#[test]
fn false_and_echo_then_or_echo_runs_or_fallback() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "false && echo x || echo y");

    assert_eq!(result.stdout, "y\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn true_or_echo_then_and_echo_runs_final_and_rhs() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "true || echo x && echo y");

    assert_eq!(result.stdout, "y\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn skipped_and_rhs_does_not_block_following_or_chain() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        HashMap::new(),
        "false && echo should-not-run || echo fallback && echo after",
    );

    assert_eq!(result.stdout, "fallback\nafter\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn skipped_or_rhs_does_not_block_following_and_chain() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        HashMap::new(),
        "true || echo should-not-run && echo after || echo fallback",
    );

    assert_eq!(result.stdout, "after\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn executed_failure_in_mixed_chain_runs_following_or_rhs() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "true && false || echo recovered");

    assert_eq!(result.stdout, "recovered\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn executed_success_in_mixed_chain_runs_following_and_rhs() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "false || true && echo continued");

    assert_eq!(result.stdout, "continued\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn and_long_chain_runs_all_when_each_succeeds() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo a && echo b && echo c");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "a\nb\nc\n");
}

#[test]
fn and_long_chain_short_circuits_on_first_failure() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo a && false && echo c");

    assert_ne!(result.exit_code, 0, "must propagate failure");
    assert_eq!(
        result.stdout, "a\n",
        "echo c must not run after false; stdout={:?}",
        result.stdout
    );
    assert!(!result.stdout.contains("c"));
}

#[test]
fn semicolon_runs_both_unconditionally() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo a ; echo b");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(
        result.stdout, "a\nb\n",
        "both sides of ';' must run, got {:?}",
        result.stdout
    );
}

#[test]
fn semicolon_runs_rhs_even_when_lhs_fails() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "false ; echo after");

    // POSIX: ';' has no short-circuit; the final exit code is the rhs's.
    assert_eq!(
        result.exit_code, 0,
        "after ';' the exit code is the rhs's; stderr={:?}",
        result.stderr
    );
    assert!(
        result.stdout.contains("after"),
        "rhs of ';' must run when lhs fails, got stdout={:?}",
        result.stdout
    );
}

#[test]
fn semicolon_chain_runs_all_three() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo a ; echo b ; echo c");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "a\nb\nc\n");
}

#[test]
fn parser_semicolon_splits_into_separate_items() {
    let line = crate::parse("echo a ; echo b");
    assert_eq!(
        line.items.len(),
        2,
        "';' must split into two items, got {:?}",
        line.items
    );
    assert_eq!(line.items[0].pipeline.commands[0].program, "echo");
    assert_eq!(line.items[1].pipeline.commands[0].program, "echo");
}

#[test]
fn parser_newline_splits_into_separate_items() {
    let line = crate::parse("echo a\necho b");
    assert_eq!(
        line.items.len(),
        2,
        "newline must split into two items, got {:?}",
        line.items
    );
    assert_eq!(line.items[0].pipeline.commands[0].program, "echo");
    assert_eq!(line.items[1].pipeline.commands[0].program, "echo");
}

#[test]
fn newline_runs_rhs_after_lhs_like_semicolon() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo a\necho b");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "a\nb\n");
}

#[test]
fn mixed_semicolon_and_and_chain_executes_correctly() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    // false fails, so '&& echo skipped' is skipped; ';' separates,
    // so 'echo end' runs unconditionally.
    let result = run_shell(vfs, HashMap::new(), "false && echo skipped ; echo end");

    assert!(!result.stdout.contains("skipped"));
    assert!(result.stdout.contains("end"));
}

#[test]
fn echo_hello_and_pwd_runs_both_from_observed_failure() {
    // Mirrors the failing trace: `echo hello && pwd` blew up because
    // `pwd` was missing. With pwd implemented, both must run.
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello && pwd");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "hello\n/\n");
}

#[test]
fn cd_then_ls_from_observed_failure() {
    // Mirrors the failing trace: `cd /tmp && ls`.
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/marker", b"x").unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /tmp && ls");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("marker"),
        "ls after cd /tmp must list marker, got stdout={:?}",
        result.stdout
    );
}

#[test]
fn cat_after_cd_resolves_relative_path_against_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/workspace/package.json", br#"{"name":"demo"}"#)
        .unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /workspace && cat package.json");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, r#"{"name":"demo"}"#);
}

#[test]
fn redirects_after_cd_write_relative_targets_under_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.mkdir("/workspace").unwrap();

    let result = run_shell(
        vfs,
        HashMap::new(),
        "cd /workspace && echo note > notes.txt",
    );

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(vfs.read("/workspace/notes.txt").unwrap(), b"note\n");
    assert!(
        vfs.read("/notes.txt").is_err(),
        "relative redirect must not write at VFS root"
    );
}

#[test]
fn touch_and_test_bracket_work_with_relative_paths() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.mkdir("/workspace").unwrap();

    let result = run_shell(
        vfs,
        HashMap::new(),
        "cd /workspace && mkdir -p src/lib && touch src/lib/mod.rs && [ -f src/lib/mod.rs ] && test -d src/lib",
    );

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(vfs.read("/workspace/src/lib/mod.rs").unwrap(), b"");
}

#[test]
fn printf_supports_common_string_newline_format() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), r#"printf '%s\n' hello"#);

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "hello\n");
}

#[test]
fn basename_and_dirname_cover_common_path_splitting() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        HashMap::new(),
        "basename /workspace/src/lib.rs && dirname /workspace/src/lib.rs",
    );

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "lib.rs\n/workspace/src\n");
}
