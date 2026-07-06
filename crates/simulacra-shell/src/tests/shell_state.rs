use super::*;

// ---------------------------------------------------------------------------
// papercut-1: POSIX ls flags, cd/pwd/env/which, robust pipeline parsing
// ---------------------------------------------------------------------------

#[test]
fn ls_accepts_dash_l_flag_without_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/a.txt", b"x").unwrap();
    vfs.write("/tmp/b.txt", b"y").unwrap();

    let result = run_shell(vfs, HashMap::new(), "ls -l /tmp");

    assert_eq!(
        result.exit_code, 0,
        "ls -l should succeed, got stderr={:?}",
        result.stderr
    );
    assert!(
        result.stdout.contains("a.txt") && result.stdout.contains("b.txt"),
        "ls -l /tmp must list a.txt and b.txt, got stdout={:?}",
        result.stdout
    );
    assert!(
        !result.stderr.contains("not found"),
        "stderr should not contain 'not found', got {:?}",
        result.stderr
    );
}

#[test]
fn ls_accepts_dash_a_flag_without_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/file", b"x").unwrap();

    let result = run_shell(vfs, HashMap::new(), "ls -a /tmp");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(result.stdout.contains("file"));
}

#[test]
fn ls_accepts_combined_la_flag_without_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/foo", b"x").unwrap();

    let result = run_shell(vfs, HashMap::new(), "ls -la /tmp");

    assert_eq!(
        result.exit_code, 0,
        "ls -la /tmp must succeed (no '/-la' path error), got stderr={:?}",
        result.stderr
    );
    assert!(
        result.stdout.contains("foo"),
        "ls -la /tmp must list foo, got stdout={:?}",
        result.stdout
    );
}

#[test]
fn ls_accepts_combined_al_flag_without_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/foo", b"x").unwrap();

    let result = run_shell(vfs, HashMap::new(), "ls -al /tmp");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(result.stdout.contains("foo"));
}

#[test]
fn ls_with_only_flag_lists_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/marker.txt", b"x").unwrap();

    // No path arg, only a flag: must default to cwd ('/'), not error.
    let result = run_shell(vfs, HashMap::new(), "ls -la");

    assert_eq!(
        result.exit_code, 0,
        "ls -la (no path) must succeed, got stderr={:?}",
        result.stderr
    );
    assert!(
        result.stdout.contains("marker.txt"),
        "ls -la must list root, got stdout={:?}",
        result.stdout
    );
}

#[test]
fn ls_with_unknown_flag_does_not_treat_flag_as_path() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/x", b"x").unwrap();

    // -h is not implemented; it must be ignored as a no-op flag, not parsed as
    // a path. The original bug surfaced as "ls: not found: /-h".
    let result = run_shell(vfs, HashMap::new(), "ls -h /tmp");

    assert!(
        !result.stderr.contains("/-h"),
        "ls must not interpret '-h' as a path, got stderr={:?}",
        result.stderr
    );
}

#[test]
fn pwd_prints_default_root_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "pwd");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "/\n");
}

#[test]
fn cd_changes_cwd_and_pwd_reports_it() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.mkdir("/tmp").unwrap();

    // cd and pwd must share state through the same executor invocation.
    let result = run_shell(vfs, HashMap::new(), "cd /tmp && pwd");

    assert_eq!(
        result.exit_code, 0,
        "cd /tmp && pwd must succeed, got stderr={:?}",
        result.stderr
    );
    assert_eq!(
        result.stdout, "/tmp\n",
        "after cd /tmp, pwd must report /tmp, got {:?}",
        result.stdout
    );
}

#[test]
fn cd_to_nonexistent_directory_fails_and_does_not_change_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "cd /no-such-dir && pwd");

    assert_ne!(result.exit_code, 0, "cd to missing dir must fail");
    assert!(
        result.stderr.to_lowercase().contains("no such")
            || result.stderr.to_lowercase().contains("not"),
        "stderr should mention missing dir, got {:?}",
        result.stderr
    );
    // && short-circuit means pwd does not run, so stdout is empty.
    assert_eq!(result.stdout, "");
}

#[test]
fn cd_to_file_fails() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/file.txt", b"x").unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /file.txt");

    assert_ne!(result.exit_code, 0, "cd to a file must fail");
    assert!(
        !result.stderr.is_empty(),
        "cd to a file must emit stderr, got empty"
    );
}

#[test]
fn cd_with_relative_path_resolves_against_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.mkdir("/a").unwrap();
    vfs.mkdir("/a/b").unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /a && cd b && pwd");

    assert_eq!(
        result.exit_code, 0,
        "relative cd must succeed, got stderr={:?}",
        result.stderr
    );
    assert_eq!(result.stdout, "/a/b\n");
}

#[test]
fn cd_dotdot_walks_up_one_level() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.mkdir("/a").unwrap();
    vfs.mkdir("/a/b").unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /a/b && cd .. && pwd");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "/a\n");
}

#[test]
fn cd_dot_slash_subdir_resolves() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.mkdir("/a").unwrap();
    vfs.mkdir("/a/sub").unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /a && cd ./sub && pwd");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "/a/sub\n");
}

#[test]
fn cd_dotdot_at_root_stays_at_root() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "cd .. && pwd");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "/\n");
}

#[test]
fn ls_after_cd_lists_relative_to_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/foo", b"x").unwrap();
    vfs.write("/tmp/bar", b"y").unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /tmp && ls");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("foo") && result.stdout.contains("bar"),
        "ls in /tmp must list foo and bar, got stdout={:?}",
        result.stdout
    );
}

#[test]
fn env_with_no_args_prints_environment_variables() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let mut env = HashMap::new();
    env.insert("FOO".to_string(), "bar".to_string());

    let result = run_shell(vfs, env, "env");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("FOO=bar"),
        "env output must contain FOO=bar, got {:?}",
        result.stdout
    );
}

#[test]
fn env_with_empty_environment_returns_zero_and_no_stderr() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "env");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stderr, "");
}

#[test]
fn env_after_export_shows_new_variable() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "export GREETING=hello && env");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("GREETING=hello"),
        "env after export must show GREETING=hello, got {:?}",
        result.stdout
    );
}

#[test]
fn which_resolves_known_builtin_to_its_name() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "which echo");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("echo"),
        "which echo must mention 'echo' in stdout, got {:?}",
        result.stdout
    );
}

#[test]
fn which_resolves_pwd_builtin() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "which pwd");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("pwd"),
        "which pwd must mention 'pwd' in stdout, got {:?}",
        result.stdout
    );
}

#[test]
fn which_resolves_rg_builtin() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "which rg");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("rg"),
        "which rg must mention 'rg' in stdout, got {:?}",
        result.stdout
    );
}

#[test]
fn which_unknown_command_returns_nonzero() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "which definitely_not_a_real_command");

    assert_ne!(
        result.exit_code, 0,
        "which on a missing command must return nonzero"
    );
}

#[test]
fn which_with_no_args_returns_nonzero() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "which");

    assert_ne!(
        result.exit_code, 0,
        "which with no args must return nonzero"
    );
}
