use super::*;

// ---------------------------------------------------------------------------
// SH7: wc test with exact assertion (replaces loose starts_with)
// ---------------------------------------------------------------------------

#[test]
fn wc_counts_lines_exact() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/f.txt", b"a\nb\nc\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "wc -l /f.txt");

    assert_eq!(
        result.stdout.trim(),
        "3",
        "wc -l should report exactly '3', got {:?}",
        result.stdout
    );
    assert_eq!(result.exit_code, 0);
}

#[test]
fn sleep_zero_exits_zero_with_no_output() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "sleep 0");

    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn sleep_one_is_recognized() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "sleep 1");

    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn sleep_invalid_duration_exits_nonzero_with_actionable_stderr() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "sleep not-a-duration");

    assert_eq!(result.stdout, "");
    assert_ne!(result.exit_code, 0);
    assert_ne!(result.exit_code, 127);
    assert!(
        result.stderr.contains("sleep"),
        "stderr should identify the failing command, got {:?}",
        result.stderr
    );
    assert!(
        result.stderr.contains("not-a-duration"),
        "stderr should include the invalid duration, got {:?}",
        result.stderr
    );
    assert!(
        result.stderr.to_lowercase().contains("duration")
            || result.stderr.to_lowercase().contains("interval")
            || result.stderr.to_lowercase().contains("number"),
        "stderr should explain how the duration is invalid, got {:?}",
        result.stderr
    );
}

// ---------------------------------------------------------------------------
// SH8: find test with exact path assertions
// ---------------------------------------------------------------------------

#[test]
fn find_lists_files_with_full_paths() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/d/a.txt", b"a").unwrap();
    vfs.write("/d/sub/b.txt", b"b").unwrap();

    let result = run_shell(vfs, HashMap::new(), "find /d");

    let lines: Vec<&str> = result.stdout.trim().lines().collect();
    // find should list the directory itself and all files/subdirs
    assert!(
        lines.contains(&"/d"),
        "find output should include the search root '/d', got {:?}",
        lines
    );
    assert!(
        lines.contains(&"/d/a.txt"),
        "find output should include '/d/a.txt', got {:?}",
        lines
    );
    assert!(
        lines.contains(&"/d/sub"),
        "find output should include '/d/sub', got {:?}",
        lines
    );
    assert!(
        lines.contains(&"/d/sub/b.txt"),
        "find output should include '/d/sub/b.txt', got {:?}",
        lines
    );
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// GS3: Escaped quotes (implementation bug)
// ---------------------------------------------------------------------------

#[test]
fn parser_escaped_double_quote_inside_double_quotes() {
    // In a POSIX shell: echo "hello \"world\"" should produce: hello "world"
    let line = crate::parse(r#"echo "hello \"world\"""#);
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.program, "echo");
    assert_eq!(cmd.args, vec!["hello \"world\""]);
}

#[test]
fn executor_escaped_quotes_in_echo() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), r#"echo "hello \"world\"""#);

    assert_eq!(result.stdout, "hello \"world\"\n");
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// GS4: Single quotes suppress variable expansion (implementation bug)
// ---------------------------------------------------------------------------

#[test]
fn single_quotes_suppress_variable_expansion() {
    // In a POSIX shell: echo '$HOME' should produce the literal string $HOME
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let mut env = HashMap::new();
    env.insert("HOME".to_string(), "/home/simulacra".to_string());

    let result = run_shell(vfs, env, "echo '$HOME'");

    assert_eq!(
        result.stdout, "$HOME\n",
        "single-quoted $HOME should be literal, not expanded"
    );
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// GS5: Pipeline stderr from intermediate stages (implementation bug)
// ---------------------------------------------------------------------------

#[test]
fn pipeline_preserves_intermediate_stderr() {
    // When a command in the middle of a pipeline writes to stderr,
    // the final result should include that stderr (or at least not silently discard it).
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    // nonexistent_cmd produces stderr ("command not found") with exit 127
    // piped into echo which succeeds — but the stderr from stage 1 should be preserved
    let result = run_shell(vfs, HashMap::new(), "nonexistent_cmd | echo ok");

    assert!(
        !result.stderr.is_empty(),
        "stderr from earlier pipeline stage should not be silently discarded"
    );
}

// ---------------------------------------------------------------------------
// S002 gap-fill: variable expansion
// ---------------------------------------------------------------------------

#[test]
fn dollar_var_expansion_replaces_with_env_value() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let mut env = HashMap::new();
    env.insert("GREETING".to_string(), "hi there".to_string());

    let result = run_shell(vfs, env, "echo $GREETING");

    assert_eq!(result.stdout, "hi there\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn undefined_variable_expands_to_empty_string() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo $UNDEFINED_VAR");

    assert_eq!(result.stdout, "\n");
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// S002 gap-fill: command substitution
// ---------------------------------------------------------------------------

#[test]
fn command_substitution_captures_stdout() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo $(echo inner)");

    assert_eq!(result.stdout, "inner\n");
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// S002 gap-fill: builtins (mkdir, cp, mv, rm, head, tail, sed, wc, find,
//                          sort, uniq, cut, tr, tee)
// ---------------------------------------------------------------------------

#[test]
fn mkdir_creates_directory_that_ls_can_list() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    run_shell(vfs, HashMap::new(), "mkdir /mydir");
    let result = run_shell(vfs, HashMap::new(), "echo x > /mydir/f.txt && ls /mydir");

    assert_eq!(result.stdout, "f.txt\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn cp_copies_file_content() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    run_shell(vfs, HashMap::new(), "echo hello > /a.txt");
    run_shell(vfs, HashMap::new(), "cp /a.txt /b.txt");
    let result = run_shell(vfs, HashMap::new(), "cat /b.txt");

    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn mv_moves_file_so_original_is_gone() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    run_shell(vfs, HashMap::new(), "echo data > /src.txt");
    run_shell(vfs, HashMap::new(), "mv /src.txt /dst.txt");

    let cat_dst = run_shell(vfs, HashMap::new(), "cat /dst.txt");
    assert_eq!(cat_dst.stdout, "data\n");

    let cat_src = run_shell(vfs, HashMap::new(), "cat /src.txt");
    assert_ne!(cat_src.exit_code, 0);
}

#[test]
fn rm_removes_file() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    run_shell(vfs, HashMap::new(), "echo x > /del.txt");
    run_shell(vfs, HashMap::new(), "rm /del.txt");

    let result = run_shell(vfs, HashMap::new(), "cat /del.txt");
    assert_ne!(result.exit_code, 0);
}

#[test]
fn head_returns_first_lines() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/lines.txt", b"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n")
        .unwrap();
    let result = run_shell(vfs, HashMap::new(), "head -n 3 /lines.txt");

    assert_eq!(result.stdout, "a\nb\nc\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn tail_returns_last_lines() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/lines.txt", b"a\nb\nc\nd\ne\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "tail -n 2 /lines.txt");

    assert_eq!(result.stdout, "d\ne\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn sed_substitution_replaces_text() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/f.txt", b"hello world\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "cat /f.txt | sed s/hello/goodbye/");

    assert_eq!(result.stdout, "goodbye world\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn wc_counts_lines() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/f.txt", b"a\nb\nc\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "wc -l /f.txt");

    assert!(
        result.stdout.trim().starts_with("3"),
        "wc -l should report 3 lines, got {:?}",
        result.stdout
    );
    assert_eq!(result.exit_code, 0);
}

#[test]
fn find_lists_files_recursively() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/d/a.txt", b"a").unwrap();
    vfs.write("/d/sub/b.txt", b"b").unwrap();

    let result = run_shell(vfs, HashMap::new(), "find /d");

    assert!(result.stdout.contains("a.txt"), "find should list a.txt");
    assert!(result.stdout.contains("b.txt"), "find should list b.txt");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn sort_orders_lines_alphabetically() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/f.txt", b"cherry\napple\nbanana\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "cat /f.txt | sort");

    assert_eq!(result.stdout, "apple\nbanana\ncherry\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn sort_dash_r_orders_lines_reverse_alphabetically() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "printf 'b\\na\\nc\\n' | sort -r");

    assert_eq!(result.stdout, "c\nb\na\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn sort_dash_n_orders_signed_integer_prefixes_numerically() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        HashMap::new(),
        "printf '10 x\\n-2 y\\n9 z\\n' | sort -n",
    );

    assert_eq!(result.stdout, "-2 y\n9 z\n10 x\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn uniq_removes_adjacent_duplicates() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/f.txt", b"a\na\nb\nb\nb\nc\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "cat /f.txt | uniq");

    assert_eq!(result.stdout, "a\nb\nc\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn uniq_dash_c_counts_adjacent_runs_only() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "printf 'a\\na\\nb\\na\\n' | uniq -c");

    assert_eq!(result.stdout, "      2 a\n      1 b\n      1 a\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn cut_extracts_fields() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/f.txt", b"a:b:c\nx:y:z\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "cat /f.txt | cut -d : -f 2");

    assert_eq!(result.stdout, "b\ny\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn tr_translates_characters() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello | tr l r");

    assert_eq!(result.stdout, "herro\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn tee_writes_to_file_and_stdout() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello | tee /out.txt");

    assert_eq!(result.stdout, "hello\n");
    assert_eq!(vfs.read("/out.txt").unwrap(), b"hello\n");
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// S002 gap-fill: pipe exit code from rightmost command
// ---------------------------------------------------------------------------

#[test]
fn pipe_exit_code_comes_from_rightmost_command() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    // Left side fails (false = exit 1), right side succeeds (echo = exit 0)
    // Pipe exit code should be 0 (from rightmost)
    let result = run_shell(vfs, HashMap::new(), "false | echo ok");

    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// S002 gap-fill: VFS isolation (shell never touches real FS)
// ---------------------------------------------------------------------------
