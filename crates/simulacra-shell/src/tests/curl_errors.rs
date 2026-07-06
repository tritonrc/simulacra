use super::*;

#[test]
fn curl_connect_timeout() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl --connect-timeout 2 http://example.com",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.timeout_ms, Some(2000), "2 seconds = 2000ms");
}

#[test]
fn curl_data_implies_post() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -d 'payload' http://example.com",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "POST", "-d without -X should default to POST");
}

#[test]
fn curl_unsupported_flag() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl --compressed http://example.com",
    );

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("unsupported option '--compressed'"),
        "expected unsupported option message, got {:?}",
        result.stderr
    );
    assert!(
        result.stderr.contains("Supported:"),
        "error should list supported flags, got {:?}",
        result.stderr
    );
}

#[test]
fn curl_capability_denied() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy =
        MockShellHttpProxy::with_error(MockHttpError::CapabilityDenied("no http allowed".into()));

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl http://example.com");

    assert_eq!(result.exit_code, 1);
    assert!(
        result
            .stderr
            .contains("curl: capability denied: no http allowed"),
        "got {:?}",
        result.stderr
    );
}

#[test]
fn curl_budget_exhausted() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy =
        MockShellHttpProxy::with_error(MockHttpError::BudgetExhausted("out of tokens".into()));

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl http://example.com");

    assert_eq!(result.exit_code, 1);
    assert!(
        result
            .stderr
            .contains("curl: budget exhausted: out of tokens"),
        "got {:?}",
        result.stderr
    );
}

#[test]
fn curl_network_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy =
        MockShellHttpProxy::with_error(MockHttpError::NetworkError("connection refused".into()));

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl http://example.com");

    assert_eq!(result.exit_code, 1);
    assert!(
        result
            .stderr
            .contains("curl: network error: connection refused"),
        "got {:?}",
        result.stderr
    );
}

#[test]
fn curl_timeout_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_error(MockHttpError::Timeout);

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl http://example.com");

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("curl: operation timed out"),
        "got {:?}",
        result.stderr
    );
}

#[test]
fn curl_no_proxy() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    // Use run_shell (no proxy) — should fail with no-proxy message
    let result = run_shell(vfs, HashMap::new(), "curl http://example.com");

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("HTTP proxy"),
        "expected HTTP proxy error, got {:?}",
        result.stderr
    );
}

#[test]
fn curl_http_error_without_fail() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(404, "Not Found", "not found page");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl http://example.com/missing",
    );

    // Without -f, 404 should still be exit 0 and body in stdout
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "not found page");
}

#[test]
fn curl_data_raw_sends_body() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl --data-raw 'raw body' http://example.com",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "POST", "--data-raw should imply POST");
    assert_eq!(req.body.as_deref(), Some(b"raw body".as_slice()),);
}

#[test]
fn curl_request_long_form() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl --request PUT http://example.com/resource",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "PUT");
}

#[test]
fn curl_location_flag_accepted() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -L http://example.com/redirect",
    );

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "ok");
}

#[test]
fn curl_output_vfs_write_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "body");

    // Writing to "/" always fails with NotAFile in MemoryFs
    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl -o / http://example.com");

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("curl: /:"),
        "expected VFS write error, got {:?}",
        result.stderr
    );
}

// ---------------------------------------------------------------------------
// Wget tests
// ---------------------------------------------------------------------------
