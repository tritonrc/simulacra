use super::*;

fn write_package_json(vfs: &dyn VirtualFs) {
    vfs.write(
        "/package.json",
        br#"{"name":"simulacra-demo","scripts":{"test":"cargo test","build":"cargo build","dev":"vite","lint":"cargo clippy"}}"#,
    )
    .unwrap();
}

#[test]
fn which_resolves_jq_fidelity_builtin() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "which jq");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "jq: shell builtin\n");
    assert_eq!(result.stderr, "");
}

#[test]
fn jq_raw_scripts_keys_reads_package_json_from_vfs() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    write_package_json(vfs);

    let result = run_shell(
        vfs,
        HashMap::new(),
        "jq -r '.scripts | keys[]' package.json",
    );

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "build\ndev\nlint\ntest\n");
    assert_eq!(result.stderr, "");
}

#[test]
fn jq_raw_name_reads_package_json_from_vfs() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    write_package_json(vfs);

    let result = run_shell(vfs, HashMap::new(), "jq -r '.name' package.json");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "simulacra-demo\n");
    assert_eq!(result.stderr, "");
}

#[test]
fn jq_keys_array_reads_relative_file_operand_from_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/workspace/tmp/items.json", br#"["alpha","beta"]"#)
        .unwrap();

    let result = run_shell(
        vfs,
        HashMap::new(),
        "cd /workspace/tmp && jq -r 'keys[]' items.json",
    );

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "0\n1\n");
    assert_eq!(result.stderr, "");
}

#[test]
fn jq_dot_pretty_prints_valid_json_from_stdin() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        HashMap::new(),
        r#"printf %s '{"name":"simulacra-demo","scripts":{"build":"cargo build","test":"cargo test"}}' | jq '.'"#,
    );

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(
        result.stdout,
        "{\n  \"name\": \"simulacra-demo\",\n  \"scripts\": {\n    \"build\": \"cargo build\",\n    \"test\": \"cargo test\"\n  }\n}\n"
    );
    assert_eq!(result.stderr, "");
}

#[test]
fn jq_invalid_json_from_stdin_exits_nonzero_with_actionable_stderr() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), r#"printf %s '{"name":' | jq '.'"#);

    assert_eq!(result.stdout, "");
    assert_ne!(result.exit_code, 0);
    assert_ne!(result.exit_code, 127);
    assert!(result.stderr.contains("jq"), "stderr={:?}", result.stderr);
    assert!(
        result.stderr.to_lowercase().contains("json"),
        "stderr={:?}",
        result.stderr
    );
    assert!(
        result.stderr.to_lowercase().contains("invalid")
            || result.stderr.to_lowercase().contains("parse")
            || result.stderr.to_lowercase().contains("expected"),
        "stderr={:?}",
        result.stderr
    );
}
