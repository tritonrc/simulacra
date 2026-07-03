use super::support::*;

#[test]
fn simulacra_fs_namespace_object_keys_list_all_expected_exports() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import * as fs from "simulacra:fs";
            JSON.stringify(Object.keys(fs).sort());
            "#,
        )
        .expect("simulacra:fs namespace import should succeed");

    assert_eq!(
        output.result.as_deref(),
        Some(
            r#"["appendFileSync","default","existsSync","mkdirSync","readFile","readdirSync","renameSync","statSync","unlinkSync","writeFile"]"#
        )
    );
}

#[test]
fn simulacra_fs_namespace_get_own_property_names_include_all_exports() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import * as fs from "simulacra:fs";
            JSON.stringify(Object.getOwnPropertyNames(fs).sort());
            "#,
        )
        .expect("simulacra:fs namespace import should succeed");

    assert_eq!(
        output.result.as_deref(),
        Some(
            r#"["appendFileSync","default","existsSync","mkdirSync","readFile","readdirSync","renameSync","statSync","unlinkSync","writeFile"]"#
        )
    );
}

#[test]
fn simulacra_fs_namespace_read_file_export_has_function_type() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import * as fs from "simulacra:fs";
            typeof fs.readFile;
            "#,
        )
        .expect("simulacra:fs namespace import should succeed");

    assert_eq!(output.result.as_deref(), Some("function"));
}

#[test]
fn simulacra_fs_namespace_keys_do_not_expose_malformed_pointer_entries() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import * as fs from "simulacra:fs";
            JSON.stringify(Object.keys(fs));
            "#,
        )
        .expect("simulacra:fs namespace import should succeed");

    let keys = output.result.unwrap_or_default();
    assert!(
        !keys.contains("0x") && !keys.contains("ptr") && !keys.contains("__"),
        "expected Object.keys(ns) to contain export names, got {keys}"
    );
}

#[test]
fn simulacra_fs_named_import_style_is_supported() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { readFile, writeFile } from "simulacra:fs";
            `${typeof readFile}|${typeof writeFile}`;
            "#,
        )
        .expect("named imports from simulacra:fs should succeed");

    assert_eq!(output.result.as_deref(), Some("function|function"));
}

#[test]
fn simulacra_fs_default_import_style_is_supported() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import fs from "simulacra:fs";
            `${typeof fs.readFile}|${typeof fs.writeFile}`;
            "#,
        )
        .expect("default import from simulacra:fs should succeed");

    assert_eq!(output.result.as_deref(), Some("function|function"));
}

#[test]
fn simulacra_fs_namespace_import_style_is_supported() {
    let (runtime, vfs) = make_runtime();
    let fs: &dyn VirtualFs = vfs.as_ref();
    fs.write("/workspace/namespace.txt", b"namespace")
        .expect("seed file in memory fs");

    let output = runtime
        .eval(
            r#"
            import * as fs from "simulacra:fs";
            fs.readFile("/workspace/namespace.txt");
            "#,
        )
        .expect("namespace import from simulacra:fs should succeed");

    assert_eq!(output.result.as_deref(), Some("namespace"));
}

#[test]
fn legacy_fs_global_readfilesync_remains_available_after_native_module_migration() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            fs.writeFileSync("/workspace/legacy.txt", "still works");
            fs.readFileSync("/workspace/legacy.txt");
            "#,
        )
        .expect("legacy fs global should still work");

    assert_eq!(output.result.as_deref(), Some("still works"));
}

#[test]
fn legacy_console_global_log_remains_available_after_native_module_migration() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(r#"console.log("legacy console")"#)
        .expect("legacy console global should still work");

    assert_eq!(output.stdout, "legacy console\n");
}

#[test]
fn legacy_process_global_cwd_remains_available_after_native_module_migration() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval("process.cwd()")
        .expect("legacy process global should still work");

    assert_eq!(output.result.as_deref(), Some("/workspace"));
}

#[test]
fn s003_compatibility_smoke_test_stays_green_after_native_module_migration() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            fs.writeFileSync("/workspace/s003.txt", "compat");
            console.log(fs.readFileSync("/workspace/s003.txt"));
            process.cwd();
            "#,
        )
        .expect("S003 compatibility smoke test should still work");

    assert_eq!(output.stdout, "compat\n");
    assert_eq!(output.result.as_deref(), Some("/workspace"));
}

#[test]
fn s014_remote_and_relative_imports_stay_green_after_native_module_migration() {
    let fetcher = MockFetcher::new(vec![(
        "https://modules.invalid/value.js",
        Ok(r#"export default "remote";"#),
    )]);
    let (runtime, vfs) = make_runtime_with_fetcher(fetcher);
    let fs: &dyn VirtualFs = vfs.as_ref();
    fs.write("/workspace/child.js", br#"export default "relative";"#)
        .expect("seed child module");
    fs.write(
        "/workspace/parent.js",
        br#"
        import child from "./child.js";
        import remote from "https://modules.invalid/value.js";
        export default `${child}-${remote}`;
        "#,
    )
    .expect("seed parent module");

    let output = runtime
        .eval(
            r#"
            import value from "/workspace/parent.js";
            value;
            "#,
        )
        .expect("S014 remote and relative imports should still work");

    assert_eq!(output.result.as_deref(), Some("relative-remote"));
}

#[test]
fn built_in_module_loading_does_not_emit_additional_spans_compared_to_plain_eval() {
    let (plain_runtime, _) = make_runtime();
    let (module_runtime, _) = make_runtime();

    let (_, plain_spans) = capture_spans(|| plain_runtime.eval("1 + 1").unwrap());
    let (_, module_spans) = capture_spans(|| {
        module_runtime
            .eval(
                r#"
                import { readFile } from "simulacra:fs";
                typeof readFile;
                "#,
            )
            .unwrap()
    });

    let module_operations = span_operations(&module_spans);
    let plain_operations = span_operations(&plain_spans);
    let baseline = if plain_operations.is_empty() {
        vec!["js_execute".to_string()]
    } else {
        plain_operations
    };
    assert_eq!(
        module_operations, baseline,
        "built-in module loading should emit only the normal js execution span"
    );
}

#[test]
fn new_native_exports_are_available_after_s016() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { existsSync, mkdirSync } from "simulacra:fs";
            `${typeof existsSync}|${typeof mkdirSync}`;
            "#,
        )
        .expect("existsSync and mkdirSync should be available after S016 implementation");

    assert_eq!(output.result.as_deref(), Some("function|function"));
}
