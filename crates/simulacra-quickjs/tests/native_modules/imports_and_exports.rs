use super::support::*;

#[test]
fn simulacra_fs_module_is_registered_via_moduledef_not_synthetic_source() {
    let (runtime, vfs) = make_runtime();
    let fs: &dyn VirtualFs = vfs.as_ref();
    fs.write("/workspace/proof.txt", b"native")
        .expect("seed file");

    // Namespace import — the style that broke under synthetic modules
    let output = runtime
        .eval(
            r#"
            import * as fs from "simulacra:fs";
            const keys = Object.keys(fs).sort();
            const readResult = fs.readFile("/workspace/proof.txt");
            JSON.stringify({ keys, readResult, typeofRead: typeof fs.readFile });
            "#,
        )
        .expect("simulacra:fs namespace import should succeed");

    let parsed: serde_json::Value =
        serde_json::from_str(output.result.as_deref().unwrap()).unwrap();
    let keys = parsed["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();

    // Native ModuleDef produces clean export names
    assert!(
        keys.contains(&"readFile".to_string()),
        "expected 'readFile' in export keys, got {keys:?}"
    );
    assert!(
        keys.contains(&"writeFile".to_string()),
        "expected 'writeFile' in export keys, got {keys:?}"
    );
    assert!(
        keys.contains(&"default".to_string()),
        "expected 'default' in export keys, got {keys:?}"
    );
    // No malformed pointer strings
    for key in &keys {
        assert!(
            !key.contains("0x") && !key.starts_with("__"),
            "malformed key '{key}' suggests synthetic source, not native ModuleDef"
        );
    }
    assert_eq!(parsed["readResult"].as_str(), Some("native"));
    assert_eq!(parsed["typeofRead"].as_str(), Some("function"));
}

/// Verify `simulacra:console` is registered via native `ModuleDef` (not synthetic JS source).
///
/// Same rationale as the `simulacra:fs` test: namespace introspection must return
/// clean export names, and the `log` function must capture output correctly
/// through all import styles.
#[test]
fn simulacra_console_module_is_registered_via_moduledef_not_synthetic_source() {
    let (runtime, _) = make_runtime();

    // Namespace import — verify clean keys and working function
    let output = runtime
        .eval(
            r#"
            import * as consoleModule from "simulacra:console";
            const keys = Object.keys(consoleModule).sort();
            consoleModule.log("native-console-check");
            JSON.stringify({ keys, typeofLog: typeof consoleModule.log });
            "#,
        )
        .expect("simulacra:console namespace import should succeed");

    let parsed: serde_json::Value =
        serde_json::from_str(output.result.as_deref().unwrap()).unwrap();
    let keys = parsed["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();

    assert!(
        keys.contains(&"log".to_string()),
        "expected 'log' in export keys, got {keys:?}"
    );
    assert!(
        keys.contains(&"default".to_string()),
        "expected 'default' in export keys, got {keys:?}"
    );
    for key in &keys {
        assert!(
            !key.contains("0x") && !key.starts_with("__"),
            "malformed key '{key}' suggests synthetic source, not native ModuleDef"
        );
    }
    assert_eq!(parsed["typeofLog"].as_str(), Some("function"));
    assert_eq!(output.stdout, "native-console-check\n");
}

/// Verify `simulacra:process` is registered via native `ModuleDef` (not synthetic JS source).
///
/// Same rationale: namespace introspection returns clean export names, and
/// the `cwd`/`env`/`exit` exports function correctly through namespace import.
#[test]
fn simulacra_process_module_is_registered_via_moduledef_not_synthetic_source() {
    let mut env = HashMap::new();
    env.insert("CHECK_VAR".to_string(), "native_process".to_string());
    let (runtime, _) = make_runtime_with_env(env);

    let output = runtime
        .eval(
            r#"
            import * as processModule from "simulacra:process";
            const keys = Object.keys(processModule).sort();
            const cwdResult = processModule.cwd();
            const envVal = processModule.env.CHECK_VAR;
            JSON.stringify({ keys, cwdResult, envVal, typeofCwd: typeof processModule.cwd, typeofExit: typeof processModule.exit });
            "#,
        )
        .expect("simulacra:process namespace import should succeed");

    let parsed: serde_json::Value =
        serde_json::from_str(output.result.as_deref().unwrap()).unwrap();
    let keys = parsed["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();

    assert!(
        keys.contains(&"cwd".to_string()),
        "expected 'cwd' in export keys, got {keys:?}"
    );
    assert!(
        keys.contains(&"env".to_string()),
        "expected 'env' in export keys, got {keys:?}"
    );
    assert!(
        keys.contains(&"exit".to_string()),
        "expected 'exit' in export keys, got {keys:?}"
    );
    assert!(
        keys.contains(&"default".to_string()),
        "expected 'default' in export keys, got {keys:?}"
    );
    for key in &keys {
        assert!(
            !key.contains("0x") && !key.starts_with("__"),
            "malformed key '{key}' suggests synthetic source, not native ModuleDef"
        );
    }
    assert_eq!(parsed["cwdResult"].as_str(), Some("/workspace"));
    assert_eq!(parsed["envVal"].as_str(), Some("native_process"));
    assert_eq!(parsed["typeofCwd"].as_str(), Some("function"));
    assert_eq!(parsed["typeofExit"].as_str(), Some("function"));
}

#[test]
fn simulacra_fs_named_read_file_import_reads_from_vfs() {
    let (runtime, vfs) = make_runtime();
    let fs: &dyn VirtualFs = vfs.as_ref();
    fs.write("/workspace/test.txt", b"hello from vfs")
        .expect("seed file in memory fs");

    let output = runtime
        .eval(
            r#"
            import { readFile } from "simulacra:fs";
            readFile("/workspace/test.txt");
            "#,
        )
        .expect("simulacra:fs readFile import should succeed");

    assert_eq!(output.result.as_deref(), Some("hello from vfs"));
}

#[test]
fn simulacra_fs_named_write_file_import_writes_to_vfs() {
    let (runtime, vfs) = make_runtime();
    let fs: &dyn VirtualFs = vfs.as_ref();

    runtime
        .eval(
            r#"
            import { writeFile } from "simulacra:fs";
            writeFile("/workspace/out.txt", "hello");
            "#,
        )
        .expect("simulacra:fs writeFile import should succeed");

    assert_eq!(fs.read("/workspace/out.txt").unwrap(), b"hello");
}

#[test]
fn simulacra_fs_exists_sync_named_export_reports_vfs_presence() {
    let (runtime, vfs) = make_runtime();
    let fs: &dyn VirtualFs = vfs.as_ref();
    fs.write("/workspace/existing.txt", b"present")
        .expect("seed file in memory fs");

    let output = runtime
        .eval(
            r#"
            import { existsSync } from "simulacra:fs";
            `${existsSync("/workspace/existing.txt")}|${existsSync("/workspace/missing.txt")}`;
            "#,
        )
        .expect("simulacra:fs existsSync import should succeed");

    assert_eq!(output.result.as_deref(), Some("true|false"));
}

#[test]
fn simulacra_fs_mkdir_sync_named_export_creates_directory_in_vfs() {
    let (runtime, vfs) = make_runtime();

    runtime
        .eval(
            r#"
            import { mkdirSync } from "simulacra:fs";
            mkdirSync("/workspace/new-dir");
            "#,
        )
        .expect("simulacra:fs mkdirSync import should succeed");

    assert!(
        vfs.list_dir("/workspace")
            .unwrap()
            .iter()
            .any(|entry| entry == "new-dir"),
        "expected mkdirSync to create /workspace/new-dir"
    );
}

#[test]
fn simulacra_fs_default_export_exposes_read_and_write_methods() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import fs from "simulacra:fs";
            fs.writeFile("/workspace/default.txt", "via default");
            fs.readFile("/workspace/default.txt");
            "#,
        )
        .expect("simulacra:fs default import should succeed");

    assert_eq!(output.result.as_deref(), Some("via default"));
}

#[test]
fn simulacra_console_named_log_import_captures_stdout() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { log } from "simulacra:console";
            log("hi");
            "#,
        )
        .expect("simulacra:console log import should succeed");

    assert_eq!(output.stdout, "hi\n");
}

#[test]
fn simulacra_console_default_export_exposes_log_method() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import consoleModule from "simulacra:console";
            consoleModule.log("hello from default");
            "#,
        )
        .expect("simulacra:console default import should succeed");

    assert_eq!(output.stdout, "hello from default\n");
}

#[test]
fn simulacra_process_named_env_import_returns_host_controlled_environment_object() {
    let mut env = HashMap::new();
    env.insert("MY_VAR".to_string(), "my_value".to_string());
    let (runtime, _) = make_runtime_with_env(env);

    let output = runtime
        .eval(
            r#"
            import { env } from "simulacra:process";
            `${env.MY_VAR}|${String(env.HOME)}`;
            "#,
        )
        .expect("simulacra:process env import should succeed");

    assert_eq!(output.result.as_deref(), Some("my_value|undefined"));
}

#[test]
fn simulacra_process_named_cwd_import_returns_workspace() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { cwd } from "simulacra:process";
            cwd();
            "#,
        )
        .expect("simulacra:process cwd import should succeed");

    assert_eq!(output.result.as_deref(), Some("/workspace"));
}

#[test]
fn simulacra_process_named_exit_import_terminates_execution_with_the_given_code() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { exit } from "simulacra:process";
            console.log("before");
            exit(7);
            console.log("after");
            "#,
        )
        .expect("simulacra:process exit import should succeed");

    assert_eq!(output.stdout, "before\n");
    assert_eq!(output.exit_code, Some(7));
    assert_eq!(output.result, None);
}

#[test]
fn simulacra_process_default_export_exposes_env_cwd_and_exit() {
    let mut env = HashMap::new();
    env.insert("VISIBLE".to_string(), "yes".to_string());
    let (runtime, _) = make_runtime_with_env(env);

    let output = runtime
        .eval(
            r#"
            import processModule from "simulacra:process";
            console.log(processModule.env.VISIBLE);
            processModule.exit(9);
            processModule.cwd();
            "#,
        )
        .expect("simulacra:process default import should succeed");

    assert_eq!(output.stdout, "yes\n");
    assert_eq!(output.exit_code, Some(9));
}
