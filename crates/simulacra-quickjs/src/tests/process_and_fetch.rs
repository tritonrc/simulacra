use super::support::*;

#[test]
fn process_env_returns_host_controlled_object_not_real_env() {
    let vfs = Arc::new(MemoryFs::new());
    let mut env = HashMap::new();
    env.insert("MY_VAR".to_string(), "my_value".to_string());

    let runtime =
        JsRuntime::with_env(vfs as Arc<dyn VirtualFs>, env).expect("failed to create runtime");

    let output = runtime.eval("process.env.MY_VAR").unwrap();
    assert_eq!(output.result.as_deref(), Some("my_value"));

    // Real env vars should NOT be visible
    let output = runtime.eval("process.env.HOME").unwrap();
    assert!(
        output.result.is_none() || output.result.as_deref() == Some("undefined"),
        "real HOME env var should not be visible, got: {:?}",
        output.result
    );
}

#[test]
fn process_cwd_returns_vfs_working_directory() {
    let (runtime, _) = make_runtime();

    let output = runtime.eval("process.cwd()").unwrap();
    assert_eq!(
        output.result.as_deref(),
        Some("/workspace"),
        "default cwd should be /workspace"
    );
}

#[test]
fn process_exit_zero_terminates_js_and_returns_exit_code() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            console.log("before");
            process.exit(0);
            console.log("after");
            "#,
        )
        .unwrap();

    assert_eq!(output.stdout, "before\n");
    assert_eq!(output.exit_code, Some(0));
}

#[test]
fn process_exit_one_terminates_js_and_returns_exit_code() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            process.exit(1);
            console.log("should not run");
            "#,
        )
        .unwrap();

    assert!(output.stdout.is_empty());
    assert_eq!(output.exit_code, Some(1));
}

#[test]
fn process_exit_does_not_terminate_rust_process() {
    let (runtime, _) = make_runtime();

    // If process.exit actually killed the Rust process, this test would
    // never reach the assertion below.
    let _output = runtime.eval("process.exit(42)").unwrap();

    // We're still alive — process.exit only terminates JS, not Rust.
    // If process.exit killed the Rust process, we'd never reach here.
    let still_alive = 1 + 1;
    assert_eq!(still_alive, 2, "Rust process survived process.exit(42)");
}

#[test]
fn fetch_allowed_url_with_matching_network_capability_returns_a_response() {
    let (runtime, _) = make_runtime_with_fetch_fixtures(
        &["allowed.example.com"],
        vec![(
            "https://allowed.example.com/api",
            FetchFixture::json(200, r#"{"ok":true}"#),
        )],
    );

    let output = runtime
        .eval(
            r#"
            export {};
            const response = await fetch("https://allowed.example.com/api");
            [typeof response.text, typeof response.json, typeof response.status].join("|");
            "#,
        )
        .expect("allowed fetch should resolve with a response object");

    assert_eq!(output.result.as_deref(), Some("function|function|number"));
}

#[test]
fn fetch_denied_url_without_matching_network_capability_rejects_with_capability_error() {
    let (runtime, _) = make_runtime_with_fetch_fixtures(&[], vec![]);

    let error = execution_message(
        runtime
            .eval(
                r#"
                export {};
                await fetch("https://denied.example.com/api");
                "#,
            )
            .expect_err("denied fetch should reject"),
    );

    assert_contains_all(&error, &["capability denied", "denied.example.com"]);
}

#[test]
fn fetch_response_json_parses_the_json_body() {
    let (runtime, _) = make_runtime_with_fetch_fixtures(
        &["allowed.example.com"],
        vec![(
            "https://allowed.example.com/api",
            FetchFixture::json(200, r#"{"message":"ok"}"#),
        )],
    );

    let output = runtime
        .eval(
            r#"
            export {};
            const response = await fetch("https://allowed.example.com/api");
            JSON.stringify(await response.json());
            "#,
        )
        .expect("fetch().json() should resolve parsed JSON");

    assert_eq!(output.result.as_deref(), Some(r#"{"message":"ok"}"#));
}

#[test]
fn fetch_response_text_returns_the_body_as_a_string() {
    let (runtime, _) = make_runtime_with_fetch_fixtures(
        &["allowed.example.com"],
        vec![(
            "https://allowed.example.com/api",
            FetchFixture::text(200, "plain text body"),
        )],
    );

    let output = runtime
        .eval(
            r#"
            export {};
            const response = await fetch("https://allowed.example.com/api");
            await response.text();
            "#,
        )
        .expect("fetch().text() should resolve the response body");

    assert_eq!(output.result.as_deref(), Some("plain text body"));
}

#[test]
fn fetch_response_status_returns_the_http_status_code() {
    let (runtime, _) = make_runtime_with_fetch_fixtures(
        &["allowed.example.com"],
        vec![(
            "https://allowed.example.com/api",
            FetchFixture::text(204, ""),
        )],
    );

    let output = runtime
        .eval(
            r#"
            export {};
            const response = await fetch("https://allowed.example.com/api");
            response.status;
            "#,
        )
        .expect("fetch response should expose the HTTP status code");

    assert_eq!(output.result.as_deref(), Some("204"));
}

#[test]
fn fetch_dispatches_through_agentcell_proxy_instead_of_direct_runtime_network_access() {
    let (runtime, _) = make_runtime_with_fetch_fixtures(
        &["allowed.example.com"],
        vec![(
            "https://allowed.example.com/api",
            FetchFixture::json(202, r#"{"accepted":true}"#),
        )],
    );

    let (_, spans, _) = capture_trace(|| {
        runtime
            .eval(
                r#"
                export {};
                const response = await fetch("https://allowed.example.com/api");
                response.status;
                "#,
            )
            .expect("fetch should delegate through the AgentCell proxy");
    });

    let js_span = find_span(&spans, "js_execute");
    let fetch_span = find_span(&spans, "sandbox_http_fetch");

    assert_eq!(
        fetch_span.parent.as_deref(),
        Some(js_span.name.as_str()),
        "expected sandbox_http_fetch to be a child of js_execute, got {spans:#?}"
    );
    assert_eq!(
        fetch_span
            .fields
            .get("simulacra.http.url")
            .map(String::as_str),
        Some("https://allowed.example.com/api")
    );
    assert_eq!(
        fetch_span
            .fields
            .get("simulacra.http.method")
            .map(String::as_str),
        Some("GET")
    );
    assert_eq!(
        fetch_span
            .fields
            .get("simulacra.http.status")
            .map(String::as_str),
        Some("202")
    );
}
