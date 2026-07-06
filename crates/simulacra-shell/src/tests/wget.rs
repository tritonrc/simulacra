use super::*;

#[test]
fn wget_saves_to_vfs_file() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "csv,data,here");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget http://example.com/data.csv",
    );

    assert_eq!(result.exit_code, 0);
    // Body should be saved to file, not stdout
    assert_eq!(result.stdout, "");
    let saved = vfs.read("data.csv").expect("file should exist in VFS");
    assert_eq!(String::from_utf8(saved).unwrap(), "csv,data,here");
}

#[test]
fn wget_default_filename_index_html() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "<html></html>");

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "wget http://example.com/");

    assert_eq!(result.exit_code, 0);
    let saved = vfs.read("index.html").expect("index.html should exist");
    assert_eq!(String::from_utf8(saved).unwrap(), "<html></html>");
}

#[test]
fn wget_output_document() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "content");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget -O /workspace/out.txt http://example.com/page",
    );

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "");
    let saved = vfs
        .read("/workspace/out.txt")
        .expect("output file should exist");
    assert_eq!(String::from_utf8(saved).unwrap(), "content");
}

#[test]
fn wget_stdout_mode() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "body to stdout");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget -O - http://example.com/page",
    );

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "body to stdout");
    // No "Saving to" in stderr
    assert!(
        !result.stderr.contains("Saving to"),
        "stdout mode should not print 'Saving to', got: {:?}",
        result.stderr
    );
}

#[test]
fn wget_quiet() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "quiet body");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget -q http://example.com/data.txt",
    );

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stderr, "");
}

#[test]
fn wget_custom_header() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget --header=X-Custom:val http://example.com/page",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert!(
        req.headers
            .contains(&("X-Custom".to_string(), "val".to_string())),
        "expected X-Custom header, got {:?}",
        req.headers
    );
}

#[test]
fn wget_post_data() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget --post-data=hello http://example.com/api",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "POST");
    assert_eq!(req.body, Some(b"hello".to_vec()));
}

#[test]
fn wget_method_override() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget --method=PUT http://example.com/resource",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "PUT");
}

#[test]
fn wget_timeout() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget --timeout=3 http://example.com/slow",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.timeout_ms, Some(3000));
}

#[test]
fn wget_unsupported_flag() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget --spider http://example.com/",
    );

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("unsupported option '--spider'"),
        "expected unsupported option error, got {:?}",
        result.stderr
    );
    assert!(
        result.stderr.contains("Supported:"),
        "expected supported flags list, got {:?}",
        result.stderr
    );
}

#[test]
fn wget_capability_denied() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy =
        MockShellHttpProxy::with_error(MockHttpError::CapabilityDenied("no http allowed".into()));

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "wget http://example.com/");

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("wget: capability denied"),
        "expected capability denied error, got {:?}",
        result.stderr
    );
}

#[test]
fn wget_no_proxy() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "wget http://example.com/");

    assert_eq!(result.exit_code, 1);
    assert!(
        result
            .stderr
            .contains("network commands require HTTP proxy"),
        "expected no-proxy error, got {:?}",
        result.stderr
    );
}

#[test]
fn wget_overwrite_existing() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    // Pre-populate a file
    vfs.write("data.csv", b"old content")
        .expect("pre-populate should succeed");

    let proxy = MockShellHttpProxy::with_response(200, "OK", "new content");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget http://example.com/data.csv",
    );

    assert_eq!(result.exit_code, 0);
    let saved = vfs.read("data.csv").expect("file should exist");
    assert_eq!(
        String::from_utf8(saved).unwrap(),
        "new content",
        "wget should overwrite existing file"
    );
}

#[test]
fn wget_vfs_write_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "body");

    // Writing to "/" always fails with MemoryFs
    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget -O / http://example.com/page",
    );

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("wget: /:"),
        "expected VFS write error, got {:?}",
        result.stderr
    );
}

#[test]
fn wget_default_progress_output() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "page content");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget http://example.com/page.html",
    );

    assert_eq!(result.exit_code, 0);
    assert!(
        result.stderr.contains("Resolving"),
        "expected 'Resolving' in stderr, got {:?}",
        result.stderr
    );
    assert!(
        result.stderr.contains("HTTP request sent"),
        "expected 'HTTP request sent' in stderr, got {:?}",
        result.stderr
    );
    assert!(
        result.stderr.contains("Saving to"),
        "expected 'Saving to' in stderr, got {:?}",
        result.stderr
    );
}

#[test]
fn wget_no_check_certificate_accepted() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "secure");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget --no-check-certificate http://example.com/page",
    );

    assert_eq!(result.exit_code, 0);
}
