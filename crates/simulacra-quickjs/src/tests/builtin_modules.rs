use super::support::*;

#[test]
fn simulacra_fs_module_can_be_imported() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { readFile } from "simulacra:fs";
            typeof readFile;
            "#,
        )
        .expect("simulacra:fs import should succeed");

    assert_eq!(output.result.as_deref(), Some("function"));
}

#[test]
fn simulacra_console_module_can_be_imported() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { log } from "simulacra:console";
            typeof log;
            "#,
        )
        .expect("simulacra:console import should succeed");

    assert_eq!(output.result.as_deref(), Some("function"));
}

#[test]
fn simulacra_process_module_can_be_imported() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { env, cwd, exit } from "simulacra:process";
            [typeof env, typeof cwd, typeof exit].join("|");
            "#,
        )
        .expect("simulacra:process import should succeed");

    assert_eq!(output.result.as_deref(), Some("object|function|function"));
}

#[test]
fn bare_specifier_imports_are_rejected_with_a_clear_error() {
    let (runtime, _) = make_runtime();

    let error = execution_message(
        runtime
            .eval(
                r#"
                import foo from "bare-specifier";
                foo;
                "#,
            )
            .expect_err("bare specifier import should fail"),
    );

    assert_contains_all(
        &error,
        &[
            "Bare specifier 'bare-specifier' is not allowed",
            "Use 'simulacra:' for built-in modules or 'http(s)://' for remote modules",
        ],
    );
}

#[test]
fn require_remains_unavailable_for_simulacra_modules() {
    let (runtime, _) = make_runtime();

    let error = execution_message(
        runtime
            .eval(r#"require("simulacra:fs")"#)
            .expect_err("require() should remain unavailable"),
    );

    assert!(
        error.contains("require"),
        "expected require-related error, got: {error}"
    );
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
        .expect("simulacra:fs readFile import should work");

    assert_eq!(output.result.as_deref(), Some("hello from vfs"));
}

#[test]
fn simulacra_fs_read_file_via_proxy_delegates_through_fs_proxy() {
    let vfs = Arc::new(MemoryFs::new());
    let fs_proxy = Arc::new(MockFsProxy::new());
    fs_proxy.seed("/workspace/test.txt", b"from proxy");

    let runtime = JsRuntime::with_options(
        vfs as Arc<dyn VirtualFs>,
        std::time::Duration::from_secs(5),
        None,
        Some(fs_proxy as Arc<dyn FsProxy>),
    )
    .expect("failed to create runtime with fs proxy");

    let (_, spans, _) = capture_trace(|| {
        let output = runtime
            .eval(
                r#"
                import { readFile } from "simulacra:fs";
                readFile("/workspace/test.txt");
                "#,
            )
            .expect("simulacra:fs readFile should work through proxy");
        assert_eq!(output.result.as_deref(), Some("from proxy"));
    });

    // Verify the read went through the proxy (sandbox_read_file span), not
    // directly through VFS (vfs_read span).
    assert!(
        spans.iter().any(|span| field_matches(
            &span.fields,
            "simulacra.operation.name",
            "sandbox_read_file"
        )),
        "expected readFile to delegate through the FsProxy, got {spans:#?}"
    );
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
        .expect("simulacra:fs writeFile import should work");

    assert_eq!(fs.read("/workspace/out.txt").unwrap(), b"hello");
}

#[test]
fn simulacra_fs_write_file_via_proxy_delegates_through_fs_proxy() {
    let vfs = Arc::new(MemoryFs::new());
    let fs_proxy = Arc::new(MockFsProxy::new());

    let runtime = JsRuntime::with_options(
        vfs as Arc<dyn VirtualFs>,
        std::time::Duration::from_secs(5),
        None,
        Some(fs_proxy.clone() as Arc<dyn FsProxy>),
    )
    .expect("failed to create runtime with fs proxy");

    let (_, spans, _) = capture_trace(|| {
        runtime
            .eval(
                r#"
                import { writeFile } from "simulacra:fs";
                writeFile("/workspace/out.txt", "through proxy");
                "#,
            )
            .expect("simulacra:fs writeFile should work through proxy");
    });

    // Verify data arrived in the proxy's store, not the VFS.
    assert_eq!(
        fs_proxy.store.lock().unwrap().get("/workspace/out.txt"),
        Some(&b"through proxy".to_vec()),
        "writeFile should have written through the proxy store"
    );

    // Verify the write went through the proxy (sandbox_write_file span).
    assert!(
        spans.iter().any(|span| field_matches(
            &span.fields,
            "simulacra.operation.name",
            "sandbox_write_file"
        )),
        "expected writeFile to delegate through the FsProxy, got {spans:#?}"
    );
}

#[test]
fn missing_simulacra_module_exports_surface_module_errors() {
    let (runtime, _) = make_runtime();

    let error = execution_message(
        runtime
            .eval(
                r#"
                import { noSuchExport } from "simulacra:fs";
                noSuchExport;
                "#,
            )
            .expect_err("missing simulacra export should fail"),
    );

    assert_contains_all(&error, &["simulacra:fs", "noSuchExport"]);
}

#[test]
fn unknown_simulacra_modules_list_available_modules() {
    let (runtime, _) = make_runtime();

    let error = execution_message(
        runtime
            .eval(
                r#"
                import x from "simulacra:nonexistent";
                x;
                "#,
            )
            .expect_err("unknown simulacra module should fail"),
    );

    assert_contains_all(
        &error,
        &[
            "Unknown simulacra module: 'nonexistent'",
            "Available: fs, console, process",
        ],
    );
}

#[test]
fn remote_module_imports_fetch_and_load_when_network_capability_allows() {
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    let output = runtime
        .eval(
            r#"
            import lodash from "https://esm.sh/lodash";
            typeof lodash;
            "#,
        )
        .expect("remote module import should succeed when network capability allows it");

    assert_eq!(output.result.as_deref(), Some("function"));
}

#[test]
fn remote_module_imports_fail_with_capability_error_when_url_is_denied() {
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Err("Network access denied for module URL: 'https://esm.sh/lodash'."),
    )]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    let error = execution_message(
        runtime
            .eval(
                r#"
                import lodash from "https://esm.sh/lodash";
                lodash;
                "#,
            )
            .expect_err("remote module import should fail without network capability"),
    );

    assert_eq!(
        error,
        "Network access denied for module URL: 'https://esm.sh/lodash'."
    );
}

// TODO: Un-ignore when S011 AgentCell proxy lands. This test requires
// journal + budget enforcement fields (simulacra.journal.kind, simulacra.budget.resource)
// that are not yet emitted by the module fetch path. The span/parent assertions
// are already covered by `remote_module_fetch_creates_a_child_span_with_module_url`.
#[test]
#[ignore = "Blocked on S011: module fetch path does not yet emit journal/budget fields"]
fn remote_module_fetches_go_through_the_host_proxy_chain() {
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    let (_, spans, events) = capture_trace(|| {
        runtime
            .eval(
                r#"
                import lodash from "https://esm.sh/lodash";
                typeof lodash;
                "#,
            )
            .expect("remote module import should succeed");
    });

    let js_span = find_span(&spans, "js_execute");
    let fetch_span = find_span(&spans, "module_fetch");

    assert_eq!(
        fetch_span.parent.as_deref(),
        Some(js_span.name.as_str()),
        "expected module_fetch span to be a child of js_execute, got {spans:#?}"
    );
    assert_eq!(
        fetch_span
            .fields
            .get("simulacra.module.url")
            .map(String::as_str),
        Some("https://esm.sh/lodash")
    );
    assert_eq!(
        fetch_span
            .fields
            .get("simulacra.journal.kind")
            .map(String::as_str),
        Some("HttpRequest")
    );
    assert!(
        events.iter().any(|event| {
            event.fields.contains_key("simulacra.budget.resource")
                && event.current_span.as_deref() == Some(fetch_span.name.as_str())
        }),
        "expected observable budget enforcement during remote module fetch, got {events:#?}"
    );
}

#[test]
fn remote_module_http_failures_include_the_url_and_status() {
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/not-found",
        Err("Failed to fetch module 'https://esm.sh/not-found': 404 Not Found."),
    )]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    let error = execution_message(
        runtime
            .eval(
                r#"
                import missing from "https://esm.sh/not-found";
                missing;
                "#,
            )
            .expect_err("404 module import should fail"),
    );

    assert_eq!(
        error,
        "Failed to fetch module 'https://esm.sh/not-found': 404 Not Found."
    );
}

#[test]
fn remote_module_network_errors_include_the_url_and_reason() {
    let fetcher = MockFetcher::new(vec![(
        "https://offline.invalid/pkg.js",
        Err("Failed to fetch module 'https://offline.invalid/pkg.js': connection refused."),
    )]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    let error = execution_message(
        runtime
            .eval(
                r#"
                import broken from "https://offline.invalid/pkg.js";
                broken;
                "#,
            )
            .expect_err("network error module import should fail"),
    );

    assert_contains_all(
        &error,
        &["https://offline.invalid/pkg.js", "Failed to fetch module"],
    );
}

#[test]
fn importing_the_same_remote_url_twice_uses_the_runtime_cache() {
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    // Use separate eval() calls so we're testing the runtime-level module cache,
    // not JS engine import dedup within a single module (which would collapse
    // duplicate import statements before our resolver/loader ever sees them).
    let (_, spans, _events) = capture_trace(|| {
        runtime
            .eval(
                r#"
                import first from "https://esm.sh/lodash";
                typeof first;
                "#,
            )
            .expect("first import should succeed");
        runtime
            .eval(
                r#"
                import second from "https://esm.sh/lodash";
                typeof second;
                "#,
            )
            .expect("second import (cache hit) should succeed");
    });

    let fetch_count = spans
        .iter()
        .filter(|span| {
            field_matches(&span.fields, "simulacra.operation.name", "module_fetch")
                && field_matches(
                    &span.fields,
                    "simulacra.module.url",
                    "https://esm.sh/lodash",
                )
        })
        .count();

    assert_eq!(
        fetch_count, 1,
        "same runtime should fetch a remote module once; second import should hit the cache"
    );
}
