use std::collections::HashMap;

use simulacra_shell::{CommandResult, ShellExecutor};
use simulacra_types::VirtualFs;
use simulacra_vfs::MemoryFs;

fn run_shell(vfs: &dyn VirtualFs, input: &str) -> CommandResult {
    let mut shell = ShellExecutor::new(vfs, HashMap::new(), None);
    shell.run(input)
}

#[test]
fn dev_null_reads_as_empty_file() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, "cat /dev/null && echo ok");

    assert_eq!(result.stdout, "ok\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn stderr_redirect_to_dev_null_discards_errors_without_vfs_write() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, "missing_tool 2>/dev/null");

    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 127);
    assert!(vfs.metadata("/dev/null").is_err());
}

#[test]
fn stderr_merge_stdout_can_flow_through_pipeline() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, "missing_tool 2>&1 | grep 'command not found'");

    assert_eq!(result.stdout, "command not found: missing_tool\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn failed_stderr_redirect_preserves_original_stderr() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, "missing_tool 2> /");

    assert_eq!(result.stdout, "");
    assert!(result.stderr.contains("command not found: missing_tool"));
    assert!(result.stderr.contains("redirect: /:"));
    assert_eq!(result.exit_code, 1);
}

#[test]
fn stdout_file_then_stderr_merge_writes_combined_output_to_file() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, "missing_tool > /combined.txt 2>&1");

    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 127);
    assert_eq!(
        String::from_utf8(vfs.read("/combined.txt").unwrap()).unwrap(),
        "command not found: missing_tool\n"
    );
}

#[test]
fn stderr_merge_before_stdout_file_keeps_stderr_on_stdout() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, "missing_tool 2>&1 > /stdout.txt");

    assert_eq!(result.stdout, "command not found: missing_tool\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 127);
    assert_eq!(vfs.read("/stdout.txt").unwrap(), b"");
}

#[test]
fn both_streams_redirect_to_file_with_ampersand_shorthand() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, "missing_tool &> /both.txt");

    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 127);
    assert_eq!(
        String::from_utf8(vfs.read("/both.txt").unwrap()).unwrap(),
        "command not found: missing_tool\n"
    );
}

#[test]
fn both_streams_append_with_ampersand_shorthand() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        "echo first &> /both.txt && echo second &>> /both.txt && cat /both.txt",
    );

    assert_eq!(result.stdout, "first\nsecond\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn unsupported_numeric_fd_redirect_stays_literal() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, "echo ok 3>/bad");

    assert_eq!(result.stdout, "ok 3>/bad\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
    assert!(vfs.metadata("/bad").is_err());
}

#[test]
fn stdout_can_redirect_to_stderr() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, "echo noisy 1>&2");

    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "noisy\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn append_redirect_to_dev_null_discards_without_touching_vfs() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        "echo kept > /kept.txt && echo ignored >> /dev/null && cat /kept.txt",
    );

    assert_eq!(result.stdout, "kept\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
    assert!(vfs.metadata("/dev/null").is_err());
}

#[test]
fn dev_null_device_is_visible_to_path_builtins() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        "echo data > /src && cp /src /dev/null && wc -c /dev/null && test -f /dev/null",
    );

    assert_eq!(result.stdout, "0\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn awk_prints_numbered_fields_from_piped_input() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, "printf 'a b\\nc d\\n' | awk '{print $2}'");

    assert_eq!(result.stdout, "b\nd\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn awk_supports_field_separator_and_last_field() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, "printf 'a,b,c\\nx,y,z\\n' | awk -F, '{print $NF}'");

    assert_eq!(result.stdout, "c\nz\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn heredoc_writes_file_through_redirect() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        "cat <<'EOF' > /workspace/note.txt\nalpha\nbeta\nEOF\ncat /workspace/note.txt",
    );

    assert_eq!(result.stdout, "alpha\nbeta\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn heredoc_feeds_pipeline_stdin() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, "cat <<EOF | grep beta\nalpha\nbeta\nEOF");

    assert_eq!(result.stdout, "beta\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn grep_recursive_line_numbers_support_code_search_idioms() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    run_shell(
        vfs,
        "mkdir -p /workspace/crates/tool && printf 'alpha\\nshell_exec here\\n' > /workspace/crates/tool/lib.rs",
    );
    run_shell(
        vfs,
        "mkdir -p /workspace/crates/runtime && printf 'js_exec there\\n' > /workspace/crates/runtime/prompt.rs",
    );

    let result = run_shell(vfs, "grep -rn 'shell_exec\\|js_exec' /workspace/crates");

    assert!(
        result
            .stdout
            .contains("/workspace/crates/runtime/prompt.rs:1:js_exec there")
    );
    assert!(
        result
            .stdout
            .contains("/workspace/crates/tool/lib.rs:2:shell_exec here")
    );
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn rg_supports_default_recursive_source_search() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    run_shell(vfs, "mkdir -p /workspace/crates/tool /workspace/docs");
    run_shell(
        vfs,
        "printf 'alpha\\nneedle here\\n' > /workspace/crates/tool/lib.rs",
    );
    run_shell(vfs, "printf 'doc needle\\n' > /workspace/docs/readme.md");

    let result = run_shell(vfs, "rg needle /workspace");

    assert!(
        result
            .stdout
            .contains("/workspace/crates/tool/lib.rs:2:needle here")
    );
    assert!(
        result
            .stdout
            .contains("/workspace/docs/readme.md:1:doc needle")
    );
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn rg_files_and_globs_cover_codex_style_file_listing() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    run_shell(vfs, "mkdir -p /workspace/src /workspace/docs");
    run_shell(
        vfs,
        "touch /workspace/src/lib.rs /workspace/src/main.rs /workspace/docs/readme.md",
    );

    let result = run_shell(vfs, "rg --files -g '*.rs' /workspace");

    assert_eq!(
        result.stdout,
        "/workspace/src/lib.rs\n/workspace/src/main.rs\n"
    );
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn rg_list_matching_files_supports_quick_target_selection() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    run_shell(vfs, "mkdir -p /workspace/src");
    run_shell(vfs, "printf 'needle\\n' > /workspace/src/lib.rs");
    run_shell(vfs, "printf 'other\\n' > /workspace/src/main.rs");

    let result = run_shell(vfs, "rg -l needle /workspace/src");

    assert_eq!(result.stdout, "/workspace/src/lib.rs\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn find_supports_type_name_or_groups_used_by_agents() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    run_shell(vfs, "mkdir -p /workspace/src /workspace/docs");
    run_shell(
        vfs,
        "touch /workspace/src/lib.rs /workspace/docs/readme.md /workspace/src/skip.txt",
    );

    let result = run_shell(
        vfs,
        "find /workspace -type f \\( -name '*.rs' -o -name '*.md' \\)",
    );

    assert_eq!(
        result.stdout,
        "/workspace/docs/readme.md\n/workspace/src/lib.rs\n"
    );
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn sed_dash_n_substitution_prints_only_matching_replacements() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        "printf 'keep a\\nskip\\nkeep b\\n' | sed -n 's/^keep //p'",
    );

    assert_eq!(result.stdout, "a\nb\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn grep_only_matching_perl_whitespace_pattern_extracts_following_fields() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        "printf 'left right third\\n' | grep -oP '(?<=\\s)\\S+'",
    );

    assert_eq!(result.stdout, "right\nthird\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}
