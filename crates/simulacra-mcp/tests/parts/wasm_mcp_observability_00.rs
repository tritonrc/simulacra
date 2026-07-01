use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::json;
use simulacra_hooks::{HookError, HookModule, HookPipeline, Operation, Phase, Verdict};
use simulacra_mcp::{FetchRequest, McpManager, load_wasm_mcp_module, wasm_mcp_fetch};
use simulacra_types::CapabilityToken;
use tempfile::NamedTempFile;
use tracing_subscriber::layer::SubscriberExt;

fn fixture_bytes(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read(&path).unwrap_or_else(|err| panic!("fixture {path} should be readable: {err}"))
}

fn trap_component_fixture() -> NamedTempFile {
    let mut tmp = NamedTempFile::new().expect("temp module should be created");
    tmp.write_all(&fixture_bytes("trap-mcp.wasm"))
        .expect("trap fixture bytes should be copied into temp module");
    tmp
}

/// Minimal denying `simulacra_hooks::HookModule` for the WARN-on-hook-deny test.
/// Always returns `Verdict::Deny` in `Phase::Before`, identity in `Phase::After`.
struct DenyBeforeHook;

impl HookModule for DenyBeforeHook {
    fn name(&self) -> &str {
        "deny-before"
    }

    fn invoke(
        &self,
        phase: Phase,
        _operation: Operation,
        _context: &str,
    ) -> Result<Verdict, HookError> {
        match phase {
            Phase::Before => Ok(Verdict::Deny("policy".to_string())),
            Phase::After => Ok(Verdict::continue_unchanged()),
        }
    }
}

fn deny_before_pipeline() -> HookPipeline {
    let mut p = HookPipeline::new();
    p.add(Operation::HttpRequest, std::sync::Arc::new(DenyBeforeHook));
    p
}

fn fetch_request_to(url: &str) -> FetchRequest {
    FetchRequest {
        method: "GET".to_string(),
        url: url.to_string(),
        headers: Vec::new(),
        body: Vec::new(),
    }
}

fn capability_with_mcp_tools(patterns: &[&str]) -> CapabilityToken {
    CapabilityToken {
        mcp_tools: patterns
            .iter()
            .map(|pattern| (*pattern).to_string())
            .collect(),
        ..Default::default()
    }
}

fn echo_component_fixture() -> NamedTempFile {
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/echo-mcp.wasm"
    ))
    .expect("echo-mcp fixture should be readable");
    let mut tmp = NamedTempFile::new().expect("temp module should be created");
    tmp.write_all(&fixture)
        .expect("fixture bytes should be copied into temp module");
    tmp
}

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct CapturedEvent {
    level: String,
    #[allow(dead_code)]
    current_span: Option<String>,
    fields: HashMap<String, String>,
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);
        self.spans.lock().expect("span mutex").push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields,
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

            let mut spans = self.spans.lock().expect("span mutex");
            for span in spans.iter_mut().rev() {
                if span.name == span_name {
                    for (key, value) in new_fields {
                        span.fields.insert(key, value);
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
        self.events
            .lock()
            .expect("event mutex")
            .push(CapturedEvent {
                level: event.metadata().level().to_string(),
                current_span: ctx.lookup_current().map(|span| span.name().to_string()),
                fields,
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
}

static CAPTURED_SPANS: OnceLock<Arc<Mutex<Vec<CapturedSpan>>>> = OnceLock::new();
static CAPTURED_EVENTS: OnceLock<Arc<Mutex<Vec<CapturedEvent>>>> = OnceLock::new();
static CAPTURE_INSTALL: OnceLock<()> = OnceLock::new();
static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn capture_store() -> (
    Arc<Mutex<Vec<CapturedSpan>>>,
    Arc<Mutex<Vec<CapturedEvent>>>,
) {
    CAPTURE_INSTALL.get_or_init(|| {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));

        CAPTURED_SPANS
            .set(Arc::clone(&spans))
            .expect("span capture should only install once");
        CAPTURED_EVENTS
            .set(Arc::clone(&events))
            .expect("event capture should only install once");

        let subscriber =
            tracing_subscriber::registry::Registry::default().with(CaptureLayer { spans, events });
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber should install");
    });

    (
        Arc::clone(CAPTURED_SPANS.get().expect("spans should be installed")),
        Arc::clone(CAPTURED_EVENTS.get().expect("events should be installed")),
    )
}

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    let _ = capture_store();
    TEST_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn capture_traces<T>(operation: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    let _guard = test_guard();
    let (spans, events) = capture_store();
    spans.lock().expect("span mutex").clear();
    events.lock().expect("event mutex").clear();

    let result = operation();
    let spans = spans.lock().expect("span mutex").clone();
    let events = events.lock().expect("event mutex").clone();
    (result, spans, events)
}

fn run_async<F>(future: F) -> F::Output
where
    F: Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime should build")
        .block_on(future)
}

fn field_matches(fields: &HashMap<String, String>, key: &str, expected: &str) -> bool {
    fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

struct JsonRpcTestServer {
    addr: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl JsonRpcTestServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn server_name(&self) -> &str {
        self.addr.split(':').next().unwrap_or("127.0.0.1")
    }
}

impl Drop for JsonRpcTestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn json_http_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_content_length(request: &str) -> usize {
    request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Option<String> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut expected_len = None;

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(bytes_read) => {
                request.extend_from_slice(&buffer[..bytes_read]);
                if expected_len.is_none()
                    && let Some(idx) = find_bytes(&request, b"\r\n\r\n")
                {
                    let header_end = idx + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    expected_len = Some(header_end + parse_content_length(&headers));
                }
                if expected_len.is_some_and(|len| request.len() >= len) {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if request.is_empty() {
                    return None;
                }
                break;
            }
            Err(_) => return None,
        }
    }

    if request.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&request).into_owned())
    }
}

fn spawn_json_rpc_test_server(tools_list_body: &str, tool_call_body: &str) -> JsonRpcTestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("JSON-RPC server should bind");
    listener
        .set_nonblocking(true)
        .expect("JSON-RPC server should become nonblocking");
    let addr = listener.local_addr().expect("local addr").to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                    if let Some(request) = read_http_request(&mut stream) {
                        let body = if request.contains("\"method\":\"initialize\"") {
                            json!({
                                "jsonrpc": "2.0",
                                "result": {
                                    "protocolVersion": "2024-11-05",
                                    "serverInfo": { "name": "fake-mcp", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string()
                        } else if request.contains("\"method\":\"notifications/initialized\"") {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_body.clone()
                        } else if request.contains("\"method\":\"tools/call\"") {
                            tool_call_body.clone()
                        } else {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        };
                        let _ = stream.write_all(json_http_response(&body).as_bytes());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    JsonRpcTestServer {
        addr,
        stop,
        handle: Some(handle),
    }
}

