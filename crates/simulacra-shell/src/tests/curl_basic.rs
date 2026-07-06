use super::*;

// ---------------------------------------------------------------------------
// Curl tests
// ---------------------------------------------------------------------------

#[test]
fn curl_get_returns_body_in_stdout() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "hello world");

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl http://example.com");

    assert_eq!(result.stdout, "hello world");
    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "GET");
    assert_eq!(req.url, "http://example.com");
}

#[test]
fn curl_post_with_data() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -X POST -d 'body data' http://example.com/api",
    );

    assert_eq!(result.stdout, "ok");
    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "POST");
    assert_eq!(req.body.as_deref(), Some(b"body data".as_slice()));
}

#[test]
fn curl_custom_header() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -H 'X-Custom: myval' http://example.com",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "X-Custom" && v == "myval"),
        "expected X-Custom header, got {:?}",
        req.headers
    );
}

#[test]
fn curl_multiple_headers() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -H 'X-First: one' -H 'X-Second: two' http://example.com",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "X-First" && v == "one"),
        "expected X-First header, got {:?}",
        req.headers
    );
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "X-Second" && v == "two"),
        "expected X-Second header, got {:?}",
        req.headers
    );
}

#[test]
fn curl_json_shorthand() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "{\"ok\":true}");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        r#"curl --json '{"a":1}' http://example.com/api"#,
    );

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "{\"ok\":true}");

    let req = proxy.last_request();
    assert_eq!(req.method, "POST", "--json should imply POST");
    assert_eq!(
        req.body.as_deref(),
        Some(b"{\"a\":1}".as_slice()),
        "--json body mismatch"
    );
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "Content-Type" && v == "application/json"),
        "expected Content-Type: application/json, got {:?}",
        req.headers
    );
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "Accept" && v == "application/json"),
        "expected Accept: application/json, got {:?}",
        req.headers
    );
}

#[test]
fn curl_output_to_file() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "file content here");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -o /workspace/out.txt http://example.com/data",
    );

    assert_eq!(result.exit_code, 0);
    // stdout should be empty when -o is used
    assert_eq!(result.stdout, "");
    // File should contain body
    let written = vfs.read("/workspace/out.txt").unwrap();
    assert_eq!(written, b"file content here");
    // stderr should contain transfer summary
    assert!(
        result.stderr.contains("% Total"),
        "expected transfer summary in stderr, got {:?}",
        result.stderr
    );
}

#[test]
fn curl_silent() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "body");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -s -o /workspace/out.txt http://example.com",
    );

    assert_eq!(result.exit_code, 0);
    // stderr should be empty with -s
    assert_eq!(result.stderr, "", "silent mode should suppress stderr");
    // File should still be written
    assert_eq!(vfs.read("/workspace/out.txt").unwrap(), b"body");
}

#[test]
fn curl_include_headers() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response_headers(
        200,
        "OK",
        vec![("Content-Type".to_string(), "text/html".to_string())],
        "body",
    );

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl -i http://example.com");

    assert_eq!(result.exit_code, 0);
    assert!(
        result.stdout.starts_with("HTTP/1.1 200 OK\r\n"),
        "expected HTTP status line, got {:?}",
        result.stdout
    );
    assert!(
        result.stdout.contains("Content-Type: text/html\r\n"),
        "expected Content-Type header, got {:?}",
        result.stdout
    );
    assert!(
        result.stdout.contains("\r\n\r\nbody"),
        "expected blank line then body, got {:?}",
        result.stdout
    );
}

#[test]
fn curl_fail_on_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(404, "Not Found", "nope");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -f http://example.com/missing",
    );

    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stdout, "", "-f should suppress body on error");
    assert!(
        result
            .stderr
            .contains("The requested URL returned error: 404 Not Found"),
        "expected error message, got {:?}",
        result.stderr
    );
}

#[test]
fn curl_verbose() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response_headers(
        200,
        "OK",
        vec![("X-Resp".to_string(), "val".to_string())],
        "body",
    );

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -v http://example.com/path",
    );

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "body");

    // Request line
    assert!(
        result.stderr.contains("> GET /path HTTP/1.1"),
        "expected request line in stderr, got {:?}",
        result.stderr
    );
    // Host header
    assert!(
        result.stderr.contains("> Host: example.com"),
        "expected Host in stderr, got {:?}",
        result.stderr
    );
    // Response status
    assert!(
        result.stderr.contains("< HTTP/1.1 200 OK"),
        "expected response status in stderr, got {:?}",
        result.stderr
    );
    // Response header
    assert!(
        result.stderr.contains("< X-Resp: val"),
        "expected response header in stderr, got {:?}",
        result.stderr
    );
}
