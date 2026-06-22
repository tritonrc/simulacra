//! Red tests for `specs/S003-quickjs.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use simulacra_types::VirtualFs;
use simulacra_vfs::MemoryFs;
use tracing_subscriber::layer::SubscriberExt;

use crate::{FsProxy, JsError, JsRuntime, ModuleFetcher};
use simulacra_fetch::{FetchError, FetchProxy, FetchResponse};

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
    parent: Option<String>,
}

#[derive(Debug, Clone)]
struct CapturedEvent {
    name: String,
    level: String,
    fields: HashMap<String, String>,
    current_span: Option<String>,
}

struct TraceCaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for TraceCaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);

        let parent = attrs
            .parent()
            .and_then(|parent_id| ctx.span(parent_id))
            .map(|span| span.name().to_string())
            .or_else(|| {
                if attrs.is_contextual() {
                    ctx.current_span()
                        .id()
                        .and_then(|parent_id| ctx.span(parent_id))
                        .map(|span| span.name().to_string())
                } else {
                    None
                }
            });

        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields,
            parent,
        });
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(span_ref) = ctx.span(id) {
            let span_name = span_ref.name().to_string();
            let mut new_fields = HashMap::new();
            let mut visitor = FieldVisitor(&mut new_fields);
            values.record(&mut visitor);

            let mut spans = self.spans.lock().unwrap();
            for captured in spans.iter_mut().rev() {
                if captured.name == span_name {
                    for (key, value) in new_fields {
                        captured.fields.insert(key, value);
                    }
                    break;
                }
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        event.record(&mut visitor);

        let current_span = ctx
            .current_span()
            .id()
            .and_then(|id| ctx.span(id))
            .map(|span| span.name().to_string());

        self.events.lock().unwrap().push(CapturedEvent {
            name: event.metadata().name().to_string(),
            level: event.metadata().level().as_str().to_string(),
            fields,
            current_span,
        });
    }
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

/// An FsProxy that emits `sandbox_read_file`/`sandbox_write_file` spans
/// and delegates to an in-memory store (not a VFS that emits its own spans).
struct MockFsProxy {
    store: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl MockFsProxy {
    fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Seed a file into the mock store (for test setup).
    fn seed(&self, path: &str, data: &[u8]) {
        self.store
            .lock()
            .unwrap()
            .insert(path.to_string(), data.to_vec());
    }
}

impl FsProxy for MockFsProxy {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        let _span = tracing::info_span!(
            "sandbox_read_file",
            simulacra.operation.name = "sandbox_read_file",
            path = %path,
        )
        .entered();
        self.store
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| format!("file not found: {path}"))
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        let _span = tracing::info_span!(
            "sandbox_write_file",
            simulacra.operation.name = "sandbox_write_file",
            path = %path,
        )
        .entered();
        self.store
            .lock()
            .unwrap()
            .insert(path.to_string(), data.to_vec());
        Ok(())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        let store = self.store.lock().unwrap();
        let prefix = if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        };
        let mut entries: Vec<String> = store
            .keys()
            .filter_map(|k| {
                k.strip_prefix(&prefix).and_then(|rest| {
                    let name = rest.split('/').next()?;
                    if name.is_empty() {
                        None
                    } else {
                        Some(name.to_string())
                    }
                })
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        entries.sort();
        Ok(entries)
    }

    fn stat(&self, path: &str) -> Result<(bool, bool, u64), String> {
        let store = self.store.lock().unwrap();
        if let Some(data) = store.get(path) {
            Ok((true, false, data.len() as u64))
        } else {
            // Check if it's a directory (any key starts with path/)
            let prefix = if path.ends_with('/') {
                path.to_string()
            } else {
                format!("{path}/")
            };
            if store.keys().any(|k| k.starts_with(&prefix)) {
                Ok((false, true, 0))
            } else {
                Err(format!("not found: {path}"))
            }
        }
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        self.store
            .lock()
            .unwrap()
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| format!("not found: {path}"))
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let mut store = self.store.lock().unwrap();
        let data = store
            .remove(from)
            .ok_or_else(|| format!("not found: {from}"))?;
        store.insert(to.to_string(), data);
        Ok(())
    }

    fn exists(&self, path: &str) -> Result<bool, String> {
        let store = self.store.lock().unwrap();
        if store.contains_key(path) {
            return Ok(true);
        }
        let prefix = if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        };
        Ok(store.keys().any(|k| k.starts_with(&prefix)))
    }

    fn mkdir(&self, _path: &str) -> Result<(), String> {
        Ok(())
    }
}

struct VfsBackedFsProxy {
    vfs: Arc<MemoryFs>,
}

impl VfsBackedFsProxy {
    fn new(vfs: Arc<MemoryFs>) -> Self {
        Self { vfs }
    }
}

impl FsProxy for VfsBackedFsProxy {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        let _span = tracing::info_span!(
            "vfs_read",
            simulacra.operation.name = "vfs_read",
            path = %path,
        )
        .entered();
        self.vfs.read(path).map_err(|e| e.to_string())
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        let _span = tracing::info_span!(
            "vfs_write",
            simulacra.operation.name = "vfs_write",
            path = %path,
        )
        .entered();
        self.vfs.write(path, data).map_err(|e| e.to_string())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        self.vfs.list_dir(path).map_err(|e| e.to_string())
    }

    fn stat(&self, path: &str) -> Result<(bool, bool, u64), String> {
        let meta = self.vfs.metadata(path).map_err(|e| e.to_string())?;
        Ok((meta.is_file, meta.is_dir, meta.size))
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        self.vfs.remove(path).map_err(|e| e.to_string())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let data = self.vfs.read(from).map_err(|e| e.to_string())?;
        if let Some(parent) = std::path::Path::new(to).parent() {
            let parent = parent.to_string_lossy();
            if !parent.is_empty() && parent != "/" {
                let _ = self.vfs.mkdir(&parent);
            }
        }
        self.vfs.write(to, &data).map_err(|e| e.to_string())?;
        self.vfs.remove(from).map_err(|e| e.to_string())
    }

    fn exists(&self, path: &str) -> Result<bool, String> {
        Ok(self.vfs.exists(path))
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.vfs.mkdir(path).map_err(|e| e.to_string())
    }
}

/// A mock fetch proxy that returns canned responses for allowed URLs.
struct MockFetchProxy {
    fixtures: HashMap<String, FetchFixture>,
    allowed_hosts: Vec<String>,
}

impl FetchProxy for MockFetchProxy {
    fn fetch(
        &self,
        url: &str,
        method: &str,
        _headers: &[(String, String)],
        _body: Option<&[u8]>,
        _timeout_ms: Option<u64>,
    ) -> Result<FetchResponse, FetchError> {
        // Emit a span mimicking what AgentCellFetchProxy would emit via fetch_http_inner
        tracing::callsite::rebuild_interest_cache();
        let span = tracing::info_span!(
            "sandbox_http_fetch",
            simulacra.operation.name = "sandbox_http_fetch",
            simulacra.http.url = %url,
            simulacra.http.method = %method,
            simulacra.http.status = tracing::field::Empty,
        );
        let _guard = span.enter();

        // Check if the URL's host is allowed
        let host = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .and_then(|rest| rest.split('/').next())
            .unwrap_or("");

        if !self.allowed_hosts.iter().any(|h| h == host) {
            return Err(FetchError::CapabilityDenied(format!(
                "network access to {host} is not allowed"
            )));
        }

        if let Some(fixture) = self.fixtures.get(url) {
            tracing::Span::current().record("simulacra.http.status", fixture.status);
            Ok(FetchResponse {
                status: fixture.status,
                status_text: String::new(),
                headers: fixture
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body: fixture.body.as_bytes().to_vec(),
                url: url.to_string(),
                redirected: false,
            })
        } else {
            Err(FetchError::NetworkError(format!(
                "MockFetchProxy: no fixture for '{url}'"
            )))
        }
    }
}

fn make_runtime() -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let runtime = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    (runtime, vfs)
}

fn make_runtime_with_vfs_proxy(vfs: Arc<MemoryFs>) -> JsRuntime {
    let proxy: Arc<dyn FsProxy> = Arc::new(VfsBackedFsProxy::new(Arc::clone(&vfs)));
    JsRuntime::with_options(
        vfs.clone() as Arc<dyn VirtualFs>,
        std::time::Duration::from_secs(5),
        None,
        Some(proxy),
    )
    .expect("failed to create runtime")
}

/// A mock fetcher for testing remote module imports.
struct MockFetcher {
    responses: HashMap<String, Result<String, String>>,
}

impl MockFetcher {
    fn new(responses: Vec<(&str, Result<&str, &str>)>) -> Self {
        Self {
            responses: responses
                .into_iter()
                .map(|(url, result)| {
                    (
                        url.to_string(),
                        result.map(|s| s.to_string()).map_err(|s| s.to_string()),
                    )
                })
                .collect(),
        }
    }
}

impl ModuleFetcher for MockFetcher {
    fn fetch(&self, url: &str) -> Result<String, String> {
        self.responses
            .get(url)
            .cloned()
            .unwrap_or_else(|| Err(format!("MockFetcher: no response configured for '{url}'")))
    }
}

fn make_runtime_with_fetcher(fetcher: MockFetcher) -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let runtime = JsRuntime::with_fetcher(vfs.clone() as Arc<dyn VirtualFs>, Box::new(fetcher))
        .expect("failed to create runtime");
    (runtime, vfs)
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct FetchFixture {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static str,
}

impl FetchFixture {
    fn text(status: u16, body: &'static str) -> Self {
        Self {
            status,
            headers: vec![("content-type", "text/plain")],
            body,
        }
    }

    fn json(status: u16, body: &'static str) -> Self {
        Self {
            status,
            headers: vec![("content-type", "application/json")],
            body,
        }
    }
}

fn make_runtime_with_fetch_fixtures(
    allowed_hosts: &[&str],
    fixtures: Vec<(&str, FetchFixture)>,
) -> (JsRuntime, Arc<MemoryFs>) {
    let vfs = Arc::new(MemoryFs::new());
    let fetch_proxy = Arc::new(MockFetchProxy {
        fixtures: fixtures
            .into_iter()
            .map(|(url, fix)| (url.to_string(), fix))
            .collect(),
        allowed_hosts: allowed_hosts.iter().map(|h| h.to_string()).collect(),
    });
    let runtime = JsRuntime::with_all_options(
        vfs.clone() as Arc<dyn VirtualFs>,
        std::time::Duration::from_secs(5),
        None,
        None,
        Some(fetch_proxy as Arc<dyn simulacra_fetch::FetchProxy>),
    )
    .expect("failed to create runtime");
    (runtime, vfs)
}

fn capture_trace<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    static GLOBAL_TRACING: OnceLock<()> = OnceLock::new();
    GLOBAL_TRACING.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
        tracing::callsite::rebuild_interest_cache();
    });

    static CAPTURE_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = CAPTURE_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("capture mutex should not be poisoned");

    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let layer = TraceCaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    };
    let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
    let result = tracing::subscriber::with_default(subscriber, || {
        // Rebuild interest cache so that callsites registered on other threads
        // (where no subscriber was active) are re-evaluated against this subscriber.
        tracing::callsite::rebuild_interest_cache();
        f()
    });
    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

fn field_matches(fields: &HashMap<String, String>, key: &str, expected: &str) -> bool {
    fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

fn find_span<'a>(spans: &'a [CapturedSpan], operation: &str) -> &'a CapturedSpan {
    spans
        .iter()
        .find(|span| field_matches(&span.fields, "simulacra.operation.name", operation))
        .unwrap_or_else(|| panic!("expected {operation} span, got {spans:#?}"))
}

fn event_text(event: &CapturedEvent) -> String {
    event
        .fields
        .values()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

fn execution_message(error: JsError) -> String {
    match error {
        JsError::Execution(message) => message,
        other => panic!("expected execution error, got {other:?}"),
    }
}

fn assert_contains_all(message: &str, expected: &[&str]) {
    for needle in expected {
        assert!(
            message.contains(needle),
            "expected {message:?} to contain {needle:?}"
        );
    }
}

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

// ---------------------------------------------------------------------------
// S003: process module
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// S014 — ESM module support
// ---------------------------------------------------------------------------

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

#[test]
fn separate_runtimes_do_not_share_the_remote_module_cache() {
    let fetcher_a = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let fetcher_b = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let (runtime_a, _) = make_runtime_with_fetcher(fetcher_a);
    let (runtime_b, _) = make_runtime_with_fetcher(fetcher_b);

    let (_, spans, _) = capture_trace(|| {
        runtime_a
            .eval(
                r#"
                import lodash from "https://esm.sh/lodash";
                typeof lodash;
                "#,
            )
            .expect("first runtime import should succeed");

        runtime_b
            .eval(
                r#"
                import lodash from "https://esm.sh/lodash";
                typeof lodash;
                "#,
            )
            .expect("second runtime import should also succeed");
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
        fetch_count, 2,
        "module cache must not be shared across runtimes"
    );
}

#[test]
fn vfs_modules_can_resolve_relative_imports_within_the_virtual_filesystem() {
    let (runtime, vfs) = make_runtime();
    let fs: &dyn VirtualFs = vfs.as_ref();

    fs.write(
        "/workspace/lib/helper.js",
        br#"export function helper() { return "from helper"; }"#,
    )
    .expect("seed helper module");
    fs.write(
        "/workspace/lib/utils.js",
        br#"import { helper } from "./helper.js"; export function run() { return helper(); }"#,
    )
    .expect("seed utils module");

    let output = runtime
        .eval(
            r#"
            import { run } from "/workspace/lib/utils.js";
            run();
            "#,
        )
        .expect("vfs module import should succeed");

    assert_eq!(output.result.as_deref(), Some("from helper"));
}

#[test]
fn remote_modules_resolve_relative_imports_against_their_url() {
    let fetcher = MockFetcher::new(vec![
        (
            "https://esm.sh/pkg/index.js",
            Ok("import val from './util.js'; export default val;"),
        ),
        (
            "https://esm.sh/pkg/util.js",
            Ok("export default 'resolved relative import';"),
        ),
    ]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    let output = runtime
        .eval(
            r#"
            import value from "https://esm.sh/pkg/index.js";
            value;
            "#,
        )
        .expect("remote module with relative imports should succeed");

    assert_eq!(output.result.as_deref(), Some("resolved relative import"));
}

#[test]
fn remote_module_code_uses_the_same_execution_timeout_as_inline_code() {
    let vfs = Arc::new(MemoryFs::new());
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/spin-forever",
        Ok("export default function() { while(true) {} };"),
    )]);
    let runtime = JsRuntime::with_timeout_and_fetcher(
        vfs as Arc<dyn VirtualFs>,
        std::time::Duration::from_millis(100),
        Box::new(fetcher),
    )
    .expect("failed to create runtime");

    let error = execution_message(
        runtime
            .eval(
                r#"
                import spin from "https://esm.sh/spin-forever";
                spin();
                "#,
            )
            .expect_err("remote module infinite loop should time out"),
    );

    assert!(
        error.to_lowercase().contains("interrupt"),
        "expected timeout/interrupt error, got: {error}"
    );
}

// TODO: Un-ignore when capability checking is wired into simulacra-quickjs.
// This test needs: (1) a MockFetcher returning a module that calls fs.readFileSync
// on a restricted path, and (2) capability enforcement that denies the read.
// Currently simulacra-quickjs does not enforce per-path capabilities (S003 behavior 4).
#[test]
#[ignore = "Blocked on capability infrastructure: simulacra-quickjs has no per-path capability checks"]
fn remote_module_fs_operations_still_respect_capability_checks() {
    let (runtime, _) = make_runtime();

    let error = execution_message(
        runtime
            .eval(
                r#"
                import { readSecret } from "https://esm.sh/read-secret";
                readSecret();
                "#,
            )
            .expect_err("remote module fs access should be denied"),
    );

    assert_contains_all(&error, &["/workspace/secret.txt", "denied"]);
}

#[test]
fn transitive_remote_imports_are_checked_against_network_capabilities() {
    let fetcher = MockFetcher::new(vec![
        (
            "https://esm.sh/parent-module",
            Ok(
                "import payload from 'https://evil.example.com/payload.js'; export default payload;",
            ),
        ),
        (
            "https://evil.example.com/payload.js",
            Err("Network access denied for module URL: 'https://evil.example.com/payload.js'."),
        ),
    ]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    let error = execution_message(
        runtime
            .eval(
                r#"
                import payload from "https://esm.sh/parent-module";
                payload;
                "#,
            )
            .expect_err("transitive remote import should be denied"),
    );

    assert_eq!(
        error,
        "Network access denied for module URL: 'https://evil.example.com/payload.js'."
    );
}

#[test]
fn legacy_globals_remain_usable_alongside_simulacra_module_imports() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { readFile } from "simulacra:fs";
            fs.writeFileSync("/workspace/legacy.txt", "still works");
            console.log(readFile("/workspace/legacy.txt"));
            process.cwd();
            "#,
        )
        .expect("legacy globals should coexist with simulacra: imports");

    assert_eq!(output.stdout, "still works\n");
    assert_eq!(output.result.as_deref(), Some("/workspace"));
}

#[test]
fn legacy_scripts_continue_to_work_after_module_loading_is_enabled() {
    let (runtime, _) = make_runtime();

    runtime
        .eval(
            r#"
            import { log } from "simulacra:console";
            log("modules enabled");
            "#,
        )
        .expect("module-enabled runtime should still allow imports");

    let output = runtime
        .eval(
            r#"
            fs.writeFileSync("/workspace/plain.js.txt", "ok");
            console.log(fs.readFileSync("/workspace/plain.js.txt"));
            process.cwd();
            "#,
        )
        .expect("legacy non-import code should still work");

    assert_eq!(output.stdout, "ok\n");
    assert_eq!(output.result.as_deref(), Some("/workspace"));
}

#[test]
fn remote_module_fetch_creates_a_child_span_with_module_url() {
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    let (_, spans, _) = capture_trace(|| {
        runtime
            .eval(
                r#"
                import lodash from "https://esm.sh/lodash";
                typeof lodash;
                "#,
            )
            .expect("remote import should succeed");
    });

    let js_span = find_span(&spans, "js_execute");
    let fetch_span = find_span(&spans, "module_fetch");

    assert_eq!(fetch_span.parent.as_deref(), Some(js_span.name.as_str()));
    assert_eq!(
        fetch_span
            .fields
            .get("simulacra.module.url")
            .map(String::as_str),
        Some("https://esm.sh/lodash")
    );
}

#[test]
fn remote_module_cache_hits_emit_a_span_event_with_hit_metadata() {
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    // First eval fetches the module; second eval triggers the cache hit.
    // (Duplicate imports within the same module are deduplicated by the JS engine
    // before our resolver/loader sees them, so we need separate evals.)
    let (_, _, events) = capture_trace(|| {
        runtime
            .eval(
                r#"
                import first from "https://esm.sh/lodash";
                typeof first;
                "#,
            )
            .expect("first remote import should succeed");
        runtime
            .eval(
                r#"
                import second from "https://esm.sh/lodash";
                typeof second;
                "#,
            )
            .expect("duplicate remote import should succeed");
    });

    assert!(
        events.iter().any(|event| {
            event.fields.get("simulacra.module.cache") == Some(&"hit".to_string())
                && event.fields.get("simulacra.module.url")
                    == Some(&"https://esm.sh/lodash".to_string())
        }),
        "expected module cache hit span event, got {events:#?}"
    );
}

#[test]
fn module_resolution_failures_are_logged_at_error_with_specifier_and_reason() {
    let (runtime, _) = make_runtime();

    let (_, _, events) = capture_trace(|| {
        let _ = runtime.eval(
            r#"
            import value from "bare-specifier";
            value;
            "#,
        );
    });

    assert!(
        events.iter().any(|event| {
            event.name.contains("event")
                && event.level == "ERROR"
                && event_text(event).contains("bare-specifier")
                && event_text(event).contains("not allowed")
        }),
        "expected ERROR event for module resolution failure, got {events:#?}"
    );
}

#[test]
fn remote_module_fetches_increment_the_fetch_counter_on_cache_miss_only() {
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    // Use separate eval() calls to bypass JS engine import dedup.
    // Within a single module, duplicate `import` statements are collapsed by
    // the JS engine before our loader runs, so we'd see exactly one fetch
    // regardless of caching — making the test pass trivially.
    let (_, _, events) = capture_trace(|| {
        runtime
            .eval(
                r#"
                import first from "https://esm.sh/lodash";
                typeof first;
                "#,
            )
            .expect("first remote import should succeed");
        runtime
            .eval(
                r#"
                import second from "https://esm.sh/lodash";
                typeof second;
                "#,
            )
            .expect("second remote import (cache hit) should succeed");
    });

    let fetch_counter_events = events
        .iter()
        .filter(|event| event.fields.get("simulacra.module.fetches") == Some(&"1".to_string()))
        .count();

    assert_eq!(
        fetch_counter_events, 1,
        "expected exactly one simulacra.module.fetches increment (first eval = cache miss); \
         second eval should hit the cache and not increment"
    );
}

// TODO: Un-ignore when S011 AgentCell proxy lands. This test requires
// capability denial events with simulacra.capability.operation and
// simulacra.capability.reason fields, which are not yet emitted.
#[test]
#[ignore = "Blocked on S011: no capability denial events emitted during module fetch"]
fn remote_module_capability_denials_emit_warn_events_with_reason() {
    let (runtime, _) = make_runtime();

    let (_, _, events) = capture_trace(|| {
        let _ = runtime.eval(
            r#"
            import lodash from "https://esm.sh/lodash";
            lodash;
            "#,
        );
    });

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event.fields.get("simulacra.capability.operation")
                    == Some(&"module_fetch".to_string())
                && event.fields.contains_key("simulacra.capability.reason")
        }),
        "expected WARN capability denial event for module fetch, got {events:#?}"
    );
}

// ===========================================================================
// Tier 1: Web-standard globals [S027]
// ===========================================================================

// --- atob/btoa ---

#[test]
fn btoa_encodes_hello() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"btoa("hello")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("aGVsbG8="));
}

#[test]
fn atob_decodes_hello() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"atob("aGVsbG8=")"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("hello"));
}

#[test]
fn btoa_throws_on_non_latin1() {
    let (rt, _) = make_runtime();
    let result = rt.eval(r#"btoa("\u{1F600}")"#);
    assert!(result.is_err());
}

#[test]
fn atob_throws_on_invalid_base64() {
    let (rt, _) = make_runtime();
    let result = rt.eval(r#"atob("not valid!!!")"#);
    assert!(result.is_err());
}

#[test]
fn btoa_atob_roundtrip() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"atob(btoa("hello world 123"))"#).unwrap();
    assert_eq!(out.result.as_deref(), Some("hello world 123"));
}

// --- TextEncoder / TextDecoder ---

#[test]
fn text_encoder_encodes_hello() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        const enc = new TextEncoder();
        const bytes = enc.encode("hello");
        JSON.stringify(Array.from(bytes))
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("[104,101,108,108,111]"));
}

#[test]
fn text_encoder_decoder_roundtrip() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        const enc = new TextEncoder();
        const dec = new TextDecoder();
        dec.decode(enc.encode("hello world"))
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("hello world"));
}

// --- URL / URLSearchParams ---

#[test]
fn url_parses_components() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        const u = new URL("https://example.com:8080/path?q=1#frag");
        JSON.stringify({
            protocol: u.protocol,
            hostname: u.hostname,
            port: u.port,
            pathname: u.pathname,
            search: u.search,
            hash: u.hash,
        })
    "#,
        )
        .unwrap();
    let val: serde_json::Value = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert_eq!(val["protocol"], "https:");
    assert_eq!(val["hostname"], "example.com");
    assert_eq!(val["port"], "8080");
    assert_eq!(val["pathname"], "/path");
    assert_eq!(val["search"], "?q=1");
    assert_eq!(val["hash"], "#frag");
}

#[test]
fn url_resolves_relative() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(r#"new URL("/path", "https://base.com").href"#)
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("https://base.com/path"));
}

#[test]
fn url_search_params_get_set() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        const p = new URLSearchParams("a=1&b=2");
        p.set("c", "3");
        p.get("a") + "," + p.get("c") + "," + p.has("b")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("1,3,true"));
}

// --- structuredClone ---

#[test]
fn structured_clone_deep_copies() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        const orig = { a: { b: 1 } };
        const clone = structuredClone(orig);
        clone.a.b = 99;
        orig.a.b
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("1"));
}

// --- queueMicrotask ---

#[test]
fn queue_microtask_executes() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        (async () => {
            let ran = false;
            queueMicrotask(() => { ran = true; });
            await Promise.resolve();
            return ran;
        })()
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

// --- performance.now ---

#[test]
fn performance_now_returns_number() {
    let (rt, _) = make_runtime();
    let out = rt.eval("typeof performance.now()").unwrap();
    assert_eq!(out.result.as_deref(), Some("number"));
}

#[test]
fn performance_now_non_decreasing() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        const a = performance.now();
        const b = performance.now();
        b >= a
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

// --- setTimeout / clearTimeout ---

#[test]
fn set_timeout_executes() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        (async () => {
            let ran = false;
            setTimeout(() => { ran = true; }, 0);
            await Promise.resolve();
            return ran;
        })()
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn set_timeout_returns_numeric_id() {
    let (rt, _) = make_runtime();
    let out = rt.eval("typeof setTimeout(() => {}, 0)").unwrap();
    assert_eq!(out.result.as_deref(), Some("number"));
}

#[test]
fn clear_timeout_cancels() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        (async () => {
            let ran = false;
            const id = setTimeout(() => { ran = true; }, 0);
            clearTimeout(id);
            await Promise.resolve();
            return ran;
        })()
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("false"));
}

#[test]
fn set_timeout_nonzero_clamps_to_zero() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        (async () => {
            let ran = false;
            setTimeout(() => { ran = true; }, 100);
            await Promise.resolve();
            return ran;
        })()
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

// --- console levels ---

#[test]
fn console_error_writes_error_level() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"console.error("boom")"#).unwrap();
    assert!(out.stdout.contains("[ERROR]"));
    assert!(out.stdout.contains("boom"));
}

#[test]
fn console_warn_writes_warn_level() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"console.warn("careful")"#).unwrap();
    assert!(out.stdout.contains("[WARN]"));
    assert!(out.stdout.contains("careful"));
}

#[test]
fn console_info_writes_info_level() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"console.info("fyi")"#).unwrap();
    assert!(out.stdout.contains("[INFO]"));
    assert!(out.stdout.contains("fyi"));
}

#[test]
fn console_debug_writes_debug_level() {
    let (rt, _) = make_runtime();
    let out = rt.eval(r#"console.debug("trace")"#).unwrap();
    assert!(out.stdout.contains("[DEBUG]"));
    assert!(out.stdout.contains("trace"));
}

// ===========================================================================
// Tier 2: simulacra:path module [S027]
// ===========================================================================

#[test]
fn path_join_basic() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.join("a", "b", "c")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("a/b/c"));
}

#[test]
fn path_join_resolves_dotdot() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.join("/a", "b", "..", "c")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("/a/c"));
}

#[test]
fn path_resolve_absolute() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.resolve("a", "b")
    "#,
        )
        .unwrap();
    let result = out.result.unwrap();
    assert!(
        result.starts_with('/'),
        "resolve should produce absolute path, got: {result}"
    );
}

#[test]
fn path_dirname() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.dirname("/a/b/c")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("/a/b"));
}

#[test]
fn path_basename() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.basename("/a/b/c.txt")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("c.txt"));
}

#[test]
fn path_basename_strips_ext() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.basename("/a/b/c.txt", ".txt")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("c"));
}

#[test]
fn path_extname() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.extname("file.tar.gz")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some(".gz"));
}

#[test]
fn path_normalize() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.normalize("/a//b/../c")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("/a/c"));
}

#[test]
fn path_is_absolute() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        JSON.stringify([path.isAbsolute("/a"), path.isAbsolute("a")])
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("[true,false]"));
}

#[test]
fn path_relative() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.relative("/a/b", "/a/c")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("../c"));
}

#[test]
fn path_parse_and_format() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        const parsed = path.parse("/a/b/c.txt");
        JSON.stringify(parsed)
    "#,
        )
        .unwrap();
    let val: serde_json::Value = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert_eq!(val["root"], "/");
    assert_eq!(val["dir"], "/a/b");
    assert_eq!(val["base"], "c.txt");
    assert_eq!(val["ext"], ".txt");
    assert_eq!(val["name"], "c");
}

#[test]
fn path_format_inverse_of_parse() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.format({ dir: "/a/b", base: "c.txt" })
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("/a/b/c.txt"));
}

#[test]
fn path_sep_and_delimiter() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import path from 'simulacra:path';
        path.sep + "," + path.delimiter
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("/,:"));
}

#[test]
fn path_named_import() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import { join } from 'simulacra:path';
        join("a", "b")
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("a/b"));
}

// ---------------------------------------------------------------------------
// simulacra:crypto tests
// ---------------------------------------------------------------------------

#[test]
fn crypto_random_uuid_format() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        const u = crypto.randomUUID();
        /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(u)
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn crypto_random_uuid_unique() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        crypto.randomUUID() !== crypto.randomUUID()
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn crypto_random_bytes_length() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        crypto.randomBytes(16).length
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("16"));
}

#[test]
fn crypto_random_bytes_zero() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        crypto.randomBytes(0).length
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("0"));
}

#[test]
fn crypto_sha256_hex() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        crypto.createHash("sha256").update("hello").digest("hex")
    "#,
        )
        .unwrap();
    assert_eq!(
        out.result.as_deref(),
        Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
    );
}

#[test]
fn crypto_sha512_base64() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        crypto.createHash("sha512").update("data").digest("base64")
    "#,
        )
        .unwrap();
    let result = out.result.unwrap();
    assert!(!result.is_empty());
    assert!(
        result
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
    );
}

#[test]
fn crypto_md5_hex() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        crypto.createHash("md5").update("test").digest("hex")
    "#,
        )
        .unwrap();
    assert_eq!(
        out.result.as_deref(),
        Some("098f6bcd4621d373cade4e832627b4f6")
    );
}

#[test]
fn crypto_create_hash_unknown_throws() {
    let (rt, _) = make_runtime();
    let result = rt.eval(
        r#"
        import crypto from 'simulacra:crypto';
        crypto.createHash("unknown")
    "#,
    );
    assert!(result.is_err());
}

#[test]
fn crypto_hash_update_chainable() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        const h1 = crypto.createHash("sha256").update("a").update("b").digest("hex");
        const h2 = crypto.createHash("sha256").update("ab").digest("hex");
        h1 === h2
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn crypto_get_random_values() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import crypto from 'simulacra:crypto';
        const arr = new Uint8Array(8);
        const result = crypto.getRandomValues(arr);
        result === arr && result.length === 8
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("true"));
}

#[test]
fn crypto_get_random_values_exceeds_limit() {
    let (rt, _) = make_runtime();
    let result = rt.eval(
        r#"
        import crypto from 'simulacra:crypto';
        crypto.getRandomValues(new Uint8Array(65537))
    "#,
    );
    assert!(result.is_err());
}

#[test]
fn crypto_named_import() {
    let (rt, _) = make_runtime();
    let out = rt
        .eval(
            r#"
        import { randomUUID } from 'simulacra:crypto';
        typeof randomUUID()
    "#,
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("string"));
}

// ---------------------------------------------------------------------------
// fs completions tests
// ---------------------------------------------------------------------------

#[test]
fn fs_extended_host_functions_without_proxy_fail_instead_of_touching_raw_vfs() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/file.txt", b"data").unwrap();
    vfs.write("/workspace/a.txt", b"data").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();

    for script in [
        r#"fs.mkdirSync("/workspace/new-dir")"#,
        r#"fs.readdirSync("/workspace")"#,
        r#"fs.statSync("/workspace/file.txt")"#,
        r#"fs.unlinkSync("/workspace/file.txt")"#,
        r#"fs.renameSync("/workspace/a.txt", "/workspace/b.txt")"#,
        r#"fs.appendFileSync("/workspace/file.txt", "more")"#,
    ] {
        let error = rt
            .eval(script)
            .expect_err("filesystem access without FsProxy should fail");
        assert!(
            error
                .to_string()
                .contains("fs proxy not configured for mediated filesystem access"),
            "unexpected error for {script}: {error}"
        );
    }

    assert!(vfs.exists("/workspace/file.txt"));
    assert!(vfs.exists("/workspace/a.txt"));
    assert!(!vfs.exists("/workspace/b.txt"));
}

#[test]
fn fs_readdir_sync() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/a.txt", b"a").unwrap();
    vfs.write("/workspace/b.txt", b"b").unwrap();
    vfs.mkdir("/workspace/subdir").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    let out = rt
        .eval(r#"JSON.stringify(fs.readdirSync("/workspace").sort())"#)
        .unwrap();
    let result: Vec<String> = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert!(result.contains(&"a.txt".to_string()));
    assert!(result.contains(&"b.txt".to_string()));
    assert!(result.contains(&"subdir".to_string()));
}

#[test]
fn fs_readdir_sync_nonexistent_throws() {
    let (rt, _) = make_runtime();
    let result = rt.eval(r#"fs.readdirSync("/nonexistent")"#);
    assert!(result.is_err());
}

#[test]
fn fs_stat_sync_file() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/file.txt", b"hello").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    let out = rt
        .eval(
            r#"
        const s = fs.statSync("/workspace/file.txt");
        JSON.stringify({ isFile: s.isFile, isDirectory: s.isDirectory, size: s.size })
    "#,
        )
        .unwrap();
    let val: serde_json::Value = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert_eq!(val["isFile"], true);
    assert_eq!(val["isDirectory"], false);
    assert_eq!(val["size"], 5);
}

#[test]
fn fs_stat_sync_directory() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace/dir").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    let out = rt
        .eval(
            r#"
        const s = fs.statSync("/workspace/dir");
        JSON.stringify({ isFile: s.isFile, isDirectory: s.isDirectory })
    "#,
        )
        .unwrap();
    let val: serde_json::Value = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert_eq!(val["isFile"], false);
    assert_eq!(val["isDirectory"], true);
}

#[test]
fn fs_stat_sync_nonexistent_throws() {
    let (rt, _) = make_runtime();
    let result = rt.eval(r#"fs.statSync("/nonexistent")"#);
    assert!(result.is_err());
}

#[test]
fn fs_unlink_sync_deletes_file() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/file.txt", b"data").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    rt.eval(r#"fs.unlinkSync("/workspace/file.txt")"#).unwrap();
    assert!(!vfs.exists("/workspace/file.txt"));
}

#[test]
fn fs_unlink_sync_nonexistent_throws() {
    let (rt, _) = make_runtime();
    let result = rt.eval(r#"fs.unlinkSync("/nonexistent")"#);
    assert!(result.is_err());
}

#[test]
fn fs_rename_sync_moves_file() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/a.txt", b"data").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    rt.eval(r#"fs.renameSync("/workspace/a.txt", "/workspace/b.txt")"#)
        .unwrap();
    assert!(!vfs.exists("/workspace/a.txt"));
    assert!(vfs.exists("/workspace/b.txt"));
    assert_eq!(vfs.read("/workspace/b.txt").unwrap(), b"data");
}

#[test]
fn fs_rename_sync_nonexistent_throws() {
    let (rt, _) = make_runtime();
    let result = rt.eval(r#"fs.renameSync("/nonexistent", "/workspace/b.txt")"#);
    assert!(result.is_err());
}

#[test]
fn fs_rename_sync_creates_parent_dirs() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/a.txt", b"data").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    rt.eval(r#"fs.renameSync("/workspace/a.txt", "/workspace/sub/dir/b.txt")"#)
        .unwrap();
    assert!(vfs.exists("/workspace/sub/dir/b.txt"));
}

#[test]
fn fs_append_file_sync_appends() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/file.txt", b"hello").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    rt.eval(r#"fs.appendFileSync("/workspace/file.txt", " world")"#)
        .unwrap();
    assert_eq!(vfs.read("/workspace/file.txt").unwrap(), b"hello world");
}

#[test]
fn fs_append_file_sync_creates_file() {
    let vfs = Arc::new(MemoryFs::new());
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    rt.eval(r#"fs.appendFileSync("/workspace/new.txt", "created")"#)
        .unwrap();
    assert_eq!(vfs.read("/workspace/new.txt").unwrap(), b"created");
}
