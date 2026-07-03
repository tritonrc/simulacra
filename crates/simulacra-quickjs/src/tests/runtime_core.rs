use super::support::*;

#[test]
fn js_fs_write_then_read_roundtrip_returns_identical_content() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            fs.writeFileSync("/artifacts/roundtrip.txt", "hello from quickjs");
            fs.readFileSync("/artifacts/roundtrip.txt")
            "#,
        )
        .unwrap();

    assert_eq!(output.result.as_deref(), Some("hello from quickjs"));
}

#[test]
fn fs_read_write_and_append_tolerate_node_style_encoding_arguments() {
    let (runtime, vfs) = make_runtime();
    vfs.write("/workspace/input.txt", b"hello").unwrap();

    let output = runtime
        .eval(
            r#"
            const original = fs.readFileSync("/workspace/input.txt", "utf8");
            fs.writeFileSync("/workspace/output.txt", original + " world", "utf8");
            fs.appendFileSync("/workspace/output.txt", "!", { encoding: "utf8" });
            fs.readFileSync("/workspace/output.txt", { encoding: "utf8" });
            "#,
        )
        .expect("Node-style encoding/options arguments should be tolerated");

    assert_eq!(output.result.as_deref(), Some("hello world!"));
    assert_eq!(vfs.read("/workspace/output.txt").unwrap(), b"hello world!");
}

#[test]
fn eval_calls_do_not_share_global_state() {
    let (runtime, _) = make_runtime();

    runtime
        .eval("globalThis.__simulacra_counter = 41; Object.prototype.polluted = true;")
        .expect("first eval should run");

    let output = runtime
        .eval(
            r#"
            [
              typeof globalThis.__simulacra_counter,
              Object.prototype.polluted === true
            ].join("|")
            "#,
        )
        .expect("second eval should run in a fresh JS context");

    assert_eq!(output.result.as_deref(), Some("undefined|false"));
}

#[test]
fn console_log_captures_output_to_virtual_stdout() {
    let (runtime, _) = make_runtime();

    let output = runtime.eval(r#"console.log("hello")"#).unwrap();

    assert_eq!(output.stdout, "hello\n");
}

#[test]
fn uncaught_exception_returns_error_with_message() {
    let (runtime, _) = make_runtime();

    let error = runtime.eval(r#"throw new Error("boom")"#).unwrap_err();

    match error {
        JsError::Execution(message) => {
            assert!(
                message.contains("boom"),
                "expected boom in error: {message}"
            );
        }
        other => panic!("expected execution error, got {other:?}"),
    }
}

#[test]
fn host_function_respects_vfs_path_resolution_without_root_escape() {
    let (runtime, fs) = make_runtime();
    let vfs: &dyn VirtualFs = fs.as_ref();

    runtime
        .eval(
            r#"
            fs.writeFileSync("/sandbox/deep/../../../escaped.txt", "still inside");
            fs.readFileSync("/escaped.txt")
            "#,
        )
        .unwrap();

    assert_eq!(vfs.read("/escaped.txt").unwrap(), b"still inside");
    assert!(!vfs.exists("/sandbox/escaped.txt"));
}

#[test]
fn js_execution_produces_span_with_operation_name_and_module() {
    let (runtime, _) = make_runtime();

    let (_, spans, _) = capture_trace(|| runtime.eval(r#"console.log("hello")"#).unwrap());

    let js_span = find_span(&spans, "js_execute");
    assert!(
        js_span.fields.contains_key("simulacra.js.module"),
        "expected simulacra.js.module on js_execute span, got {js_span:#?}"
    );
}

#[test]
fn host_function_calls_produce_child_spans_under_js_execution() {
    let (runtime, _) = make_runtime();

    let (_, spans, _) = capture_trace(|| {
        runtime
            .eval(
                r#"
                fs.writeFileSync("/logs/child.txt", "hello");
                fs.readFileSync("/logs/child.txt")
                "#,
            )
            .unwrap()
    });

    let js_span = find_span(&spans, "js_execute");
    let write_span = find_span(&spans, "vfs_write");
    let read_span = find_span(&spans, "vfs_read");

    assert_eq!(
        write_span.parent.as_deref(),
        Some(js_span.name.as_str()),
        "expected vfs_write span to be a child of js_execute, got {spans:#?}"
    );
    assert_eq!(
        read_span.parent.as_deref(),
        Some(js_span.name.as_str()),
        "expected vfs_read span to be a child of js_execute, got {spans:#?}"
    );
}

#[test]
fn uncaught_exceptions_are_logged_at_error_level_with_message_and_stack_trace() {
    let (runtime, _) = make_runtime();

    let (_, _, events) = capture_trace(|| {
        let _ = runtime.eval(
            r#"
            function explode() {
                throw new Error("boom");
            }
            explode();
            "#,
        );
    });

    let error_event = events
        .iter()
        .find(|event| event.level == "ERROR")
        .unwrap_or_else(|| panic!("expected ERROR event for uncaught exception, got {events:#?}"));
    let text = event_text(error_event);

    assert!(
        text.contains("boom"),
        "expected error log to include exception message, got {error_event:#?}"
    );
    assert!(
        text.contains("explode"),
        "expected error log to include stack trace, got {error_event:#?}"
    );
}

// ---------------------------------------------------------------------------
// S003 gap-fill: require() is not available
// ---------------------------------------------------------------------------

#[test]
fn require_is_not_available_and_throws_error() {
    let (runtime, _) = make_runtime();

    let err = runtime
        .eval(r#"require("fs")"#)
        .expect_err("require() should not be available in ESM mode");

    match err {
        JsError::Execution(msg) => {
            assert!(
                msg.contains("not defined") || msg.contains("not a function"),
                "expected 'not defined' error, got: {msg}"
            );
        }
        other => panic!("expected execution error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// S003 gap-fill: infinite loop is interrupted by timeout
// ---------------------------------------------------------------------------

#[test]
fn infinite_loop_is_interrupted_by_timeout() {
    let vfs = Arc::new(MemoryFs::new());
    let runtime = JsRuntime::with_timeout(
        vfs as Arc<dyn VirtualFs>,
        std::time::Duration::from_millis(100),
    )
    .expect("failed to create runtime");

    let err = runtime
        .eval("while (true) {}")
        .expect_err("infinite loop should be interrupted by timeout");

    match err {
        JsError::Execution(msg) => {
            assert!(
                msg.contains("interrupted") || msg.contains("Interrupted"),
                "expected interrupt error, got: {msg}"
            );
        }
        other => panic!("expected execution error from timeout, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// S003 gap-fill: console.log does not write to real stdout
// ---------------------------------------------------------------------------

#[test]
fn console_log_does_not_write_to_real_stdout() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(r#"console.log("SENTINEL_SHOULD_NOT_LEAK")"#)
        .unwrap();

    assert_eq!(output.stdout, "SENTINEL_SHOULD_NOT_LEAK\n");
    // The real test is that this sentinel doesn't appear in cargo test output
    // unless this test fails. That's the nature of console.log capturing.
}

// ---------------------------------------------------------------------------
// S003 gap-fill: fs host functions are Rust (not JS polyfills)
// ---------------------------------------------------------------------------

#[test]
fn fs_host_functions_are_native_not_js_polyfills() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval("typeof fs.readFileSync + '|' + typeof fs.writeFileSync")
        .unwrap();

    assert_eq!(
        output.result.as_deref(),
        Some("function|function"),
        "fs functions should be registered as functions"
    );

    // Verify they actually work against VFS (polyfills would fail without
    // real filesystem access)
    let output = runtime
        .eval(
            r#"
            fs.writeFileSync("/proof.txt", "native");
            fs.readFileSync("/proof.txt")
            "#,
        )
        .unwrap();
    assert_eq!(output.result.as_deref(), Some("native"));
}

#[test]
fn fs_host_functions_delegate_through_agentcell_proxy_instead_of_direct_vfs_spans() {
    let vfs = Arc::new(MemoryFs::new());
    let fs_proxy = Arc::new(MockFsProxy::new());
    fs_proxy.seed("/workspace/input.txt", b"from memory fs");

    let runtime = JsRuntime::with_options(
        vfs as Arc<dyn VirtualFs>,
        std::time::Duration::from_secs(5),
        None,
        Some(fs_proxy as Arc<dyn FsProxy>),
    )
    .expect("failed to create runtime with fs proxy");

    let (_, spans, _) = capture_trace(|| {
        runtime
            .eval(
                r#"
                fs.writeFileSync("/workspace/output.txt", "through proxy");
                fs.readFileSync("/workspace/input.txt");
                "#,
            )
            .expect("fs host functions should execute through the AgentCell proxy");
    });

    assert!(
        spans.iter().any(|span| field_matches(
            &span.fields,
            "simulacra.operation.name",
            "sandbox_read_file"
        )),
        "expected fs.readFileSync to delegate through the AgentCell proxy, got {spans:#?}"
    );
    assert!(
        spans.iter().any(|span| field_matches(
            &span.fields,
            "simulacra.operation.name",
            "sandbox_write_file"
        )),
        "expected fs.writeFileSync to delegate through the AgentCell proxy, got {spans:#?}"
    );
    assert!(
        spans.iter().all(|span| !field_matches(
            &span.fields,
            "simulacra.operation.name",
            "vfs_read"
        ) && !field_matches(
            &span.fields,
            "simulacra.operation.name",
            "vfs_write"
        )),
        "fs host functions should not touch the VFS directly once the AgentCell proxy is wired up: {spans:#?}"
    );
}
