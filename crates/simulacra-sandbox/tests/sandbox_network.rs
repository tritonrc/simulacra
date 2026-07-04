mod common;
#[allow(unused_imports)]
use common::*;

#[test]
fn fetch_http_with_denied_network_capability_surfaces_operation_and_reason_to_agent() {
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"blocked");
    let harness = Harness::new(
        capability_with_network(&[], &[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .cell
        .fetch_http(&server.url("/blocked"), "GET", &[], None, None)
        .unwrap_err();

    match sandbox_error_to_expected(error) {
        ExpectedSandboxError::CapabilityDenied(denied) => {
            assert_eq!(denied.operation, "network:127.0.0.1");
            assert_eq!(denied.reason, "no network permission for 127.0.0.1");
        }
        other => panic!("expected capability denial, got {other:?}"),
    }
}

#[test]
fn fetch_http_when_turns_budget_is_exhausted_returns_budget_exhausted_and_does_not_make_a_request()
{
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"ok");
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        budget_with_overrides(1, 1, 0, 0),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .cell
        .fetch_http(&server.url("/budget"), "GET", &[], None, None)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("budget exhausted") && error.contains("turns"),
        "expected turns budget exhaustion, got {error}"
    );
    assert_eq!(
        server.request_count(),
        0,
        "budget exhaustion must short-circuit before the HTTP request starts"
    );
}

#[test]
fn fetch_http_writes_an_httprequest_journal_entry_with_method_url_and_status_after_execution() {
    let server = spawn_http_server(201, &[("content-type", "text/plain")], b"created");
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    let url = server.url("/journal");

    let response = harness
        .cell
        .fetch_http(&url, "GET", &[], None, None)
        .expect("HTTP fetch should succeed for an allowed host");

    assert_eq!(response.status, 201);
    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::HttpRequest { method, url: entry_url, status }
                    if method == "GET" && entry_url == &url && *status == 201
            )
        }),
        "expected an HttpRequest journal entry with method, URL, and status"
    );
}

#[test]
fn fetch_http_increments_used_turns_by_one() {
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"ok");
    let budget = unlimited_budget();
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    harness
        .cell
        .fetch_http(&server.url("/turns"), "GET", &[], None, None)
        .expect("HTTP fetch should consume one turn");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after,
        before + 1,
        "fetch_http must consume one turns budget unit"
    );
}

#[test]
fn list_dir_does_not_increment_used_turns() {
    let budget = budget_with_overrides(5, 2, 0, 0);
    let harness = Harness::new(
        capability(&["/workspace"], &[], false, false),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/a.txt", b"a");
    let before = budget_counter(&budget, "used_turns");

    harness
        .list_dir("/workspace")
        .expect("list_dir should not consume a turns budget unit");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after, before,
        "list_dir is metadata-only and must not increment used_turns"
    );
}

#[test]
fn execute_shell_execute_js_and_fetch_http_all_increment_used_turns_before_execution_not_after() {
    let shell_budget = unlimited_budget();
    let shell_vfs: Arc<dyn VirtualFs> = Arc::new(PanicWriteFs::new());
    let shell_http: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let shell_cell = AgentCell::new(
        Arc::clone(&shell_vfs),
        capability(&[], &["/workspace/**"], true, false),
        Arc::clone(&shell_budget),
        Arc::new(FakeJournalStorage::default()),
        shell_http,
    );

    let shell_result = catch_unwind(AssertUnwindSafe(|| {
        let _ = shell_cell.execute_shell("echo boom > /workspace/panic.txt");
    }));
    assert!(
        shell_result.is_err(),
        "the panic-on-write VFS should interrupt shell execution"
    );
    assert_eq!(
        budget_counter(&shell_budget, "used_turns"),
        1,
        "execute_shell must pay its turns cost before execution starts, even if execution panics"
    );

    let js_budget = budget_with_overrides(1, 0, 0, 0);
    let js_harness = Harness::new(
        capability_with_network(&[], &[], &["net:modules.invalid"], false, true),
        Arc::clone(&js_budget),
        Arc::new(FakeJournalStorage::default()),
    );

    let js_error = js_harness
        .execute_js(
            r#"
            import value from "https://modules.invalid/entry.js";
            value;
            "#,
        )
        .expect_err(
            "execute_js should consume the only turns budget unit before module loading begins",
        );
    assert_budget_exhausted(js_error, &["turns"], "1", "1");

    let http_budget = unlimited_budget();
    let http_harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        Arc::clone(&http_budget),
        Arc::new(FakeJournalStorage::default()),
    );

    let http_error = http_harness
        .cell
        .fetch_http("http://127.0.0.1:9/before-exec", "GET", &[], None, None)
        .expect_err("connection-refused fetch should still consume a turns budget unit");
    let http_error_text = http_error.to_string();
    assert!(
        http_error_text.contains("127.0.0.1:9"),
        "HTTP errors should still report the failed URL, got {http_error_text}"
    );
    assert_eq!(
        budget_counter(&http_budget, "used_turns"),
        1,
        "fetch_http must pay its turns cost before the request executes"
    );
}

#[test]
fn fs_readfilesync_from_js_code_routes_through_agent_cell_read_file_and_denied_paths_return_a_js_exception()
 {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/secrets/key.pem", b"secret");

    let error = harness
        .execute_js("fs.readFileSync('/secrets/key.pem')")
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("denied") && error.contains("/secrets/key.pem"),
        "expected JS fs.readFileSync denial to surface as a JS exception, got {error}"
    );
}

#[test]
fn fs_writefilesync_from_js_code_routes_through_agent_cell_write_file_and_denied_paths_return_a_js_exception()
 {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_js("fs.writeFileSync('/workspace/blocked.txt', 'blocked')")
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("denied") && error.contains("/workspace/blocked.txt"),
        "expected JS fs.writeFileSync denial to surface as a JS exception, got {error}"
    );
    assert!(
        !harness.vfs.exists("/workspace/blocked.txt"),
        "denied JS writes must not touch the underlying VFS"
    );
}

#[test]
fn simulacra_fs_readfile_and_writefile_also_route_through_agent_cell_proxy_methods() {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/secrets/key.pem", b"secret");

    let read_error = harness
        .execute_js(
            r#"
            import { readFile } from "simulacra:fs";
            readFile("/secrets/key.pem");
            "#,
        )
        .unwrap_err()
        .to_string();
    let write_error = harness
        .execute_js(
            r#"
            import { writeFile } from "simulacra:fs";
            writeFile("/workspace/blocked.txt", "blocked");
            "#,
        )
        .unwrap_err()
        .to_string();

    assert!(
        read_error.contains("denied"),
        "expected simulacra:fs readFile to route through AgentCell read checks, got {read_error}"
    );
    assert!(
        write_error.contains("denied"),
        "expected simulacra:fs writeFile to route through AgentCell write checks, got {write_error}"
    );
}

#[test]
fn console_log_does_not_route_through_the_proxy_and_writes_directly_to_the_virtual_stdout_buffer() {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let output = harness
        .execute_js("console.log('hello from js')")
        .expect("console.log should write to the JS stdout buffer");

    assert_eq!(output.stdout, "hello from js\n");
    assert_eq!(
        harness.vfs.read_count(),
        0,
        "console.log must not read the VFS"
    );
    assert_eq!(
        harness.vfs.write_count(),
        0,
        "console.log must not write the VFS"
    );
}

#[test]
fn agent_cell_provides_a_modulefetcher_impl_to_the_js_runtime_it_owns() {
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:modules.invalid"], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_js(
            r#"
            import value from "https://modules.invalid/entry.js";
            value;
            "#,
        )
        .unwrap_err()
        .to_string();

    assert!(
        !error.contains("No module fetcher configured"),
        "expected AgentCell-owned JsRuntime to install a ModuleFetcher, got {error}"
    );
}

#[test]
fn remote_module_import_triggers_modulefetcher_fetch_which_delegates_to_agent_cell_fetch_http() {
    let journal = Arc::new(FakeJournalStorage::default());
    let budget = unlimited_budget();
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:modules.invalid"], false, true),
        Arc::clone(&budget),
        Arc::clone(&journal),
    );
    let stub_url = "https://modules.invalid/entry.js";
    harness
        .cell
        .register_module_stub(stub_url, "export default 42;");
    let before = budget_counter(&budget, "used_turns");

    let output = harness.execute_js(
        r#"
        import value from "https://modules.invalid/entry.js";
        value;
        "#,
    );

    // Verify the module fetch produced a journal entry proving delegation happened
    let entries = journal.entries();
    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::HttpRequest { method, url, status }
                if method == "GET" && url == stub_url && *status == 200
        )),
        "remote module fetch must produce an HttpRequest journal entry proving delegation, got {entries:?}"
    );

    // execute_js increments +1; module fetch does not increment turns (shares the enclosing turn).
    // But the import succeeded, which is the real proof of delegation.
    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after,
        before + 1,
        "execute_js with module fetch should consume one turn total"
    );

    // Additionally verify the module actually resolved
    let output = output.expect("module import via stub should succeed");
    assert_eq!(
        output.result.as_deref(),
        Some("42"),
        "the stub module should have been fetched and evaluated"
    );
}
