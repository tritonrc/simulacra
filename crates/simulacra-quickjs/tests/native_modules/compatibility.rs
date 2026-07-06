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
            r#"["appendFileSync","default","existsSync","mkdirSync","readFile","readFileSync","readdirSync","renameSync","statSync","unlinkSync","writeFile","writeFileSync"]"#
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
            r#"["appendFileSync","default","existsSync","mkdirSync","readFile","readFileSync","readdirSync","renameSync","statSync","unlinkSync","writeFile","writeFileSync"]"#
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

#[test]
fn simulacra_fs_sync_named_import_style_is_supported() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { readFileSync, writeFileSync } from "simulacra:fs";
            writeFileSync("/workspace/sync-named.txt", "sync named");
            readFileSync("/workspace/sync-named.txt");
            "#,
        )
        .expect("sync named imports from simulacra:fs should succeed");

    assert_eq!(output.result.as_deref(), Some("sync named"));
}

#[test]
fn simulacra_fs_default_export_exposes_sync_aliases() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import fs from "simulacra:fs";
            fs.writeFileSync("/workspace/sync-default.txt", "sync default");
            fs.readFileSync("/workspace/sync-default.txt");
            "#,
        )
        .expect("simulacra:fs default export should expose sync aliases");

    assert_eq!(output.result.as_deref(), Some("sync default"));
}

#[test]
fn bare_fs_default_import_aliases_simulacra_fs() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import fs from "fs";
            fs.writeFileSync("/workspace/bare-default.txt", "bare default");
            fs.readFileSync("/workspace/bare-default.txt");
            "#,
        )
        .expect("bare fs default import should alias simulacra:fs");

    assert_eq!(output.result.as_deref(), Some("bare default"));
}

#[test]
fn bare_fs_named_import_aliases_simulacra_fs() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { readFileSync, writeFileSync } from "fs";
            writeFileSync("/workspace/bare-named.txt", "bare named");
            readFileSync("/workspace/bare-named.txt");
            "#,
        )
        .expect("bare fs named imports should alias simulacra:fs");

    assert_eq!(output.result.as_deref(), Some("bare named"));
}

#[test]
fn bare_console_alias_imports_simulacra_console() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { log } from "console";
            log("bare console");
            "#,
        )
        .expect("bare console import should alias simulacra:console");

    assert_eq!(output.stdout, "bare console\n");
}

#[test]
fn bare_process_alias_imports_simulacra_process() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import process from "process";
            process.cwd();
            "#,
        )
        .expect("bare process import should alias simulacra:process");

    assert_eq!(output.result.as_deref(), Some("/workspace"));
}

#[test]
fn bare_path_alias_imports_simulacra_path() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import path from "path";
            path.join("a", "b");
            "#,
        )
        .expect("bare path import should alias simulacra:path");

    assert_eq!(output.result.as_deref(), Some("a/b"));
}

#[test]
fn bare_crypto_alias_imports_simulacra_crypto() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { createHash } from "crypto";
            createHash("sha256").update("hello").digest("hex");
            "#,
        )
        .expect("bare crypto import should alias simulacra:crypto");

    assert_eq!(
        output.result.as_deref(),
        Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
    );
}
