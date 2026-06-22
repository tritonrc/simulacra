#![allow(clippy::type_complexity)]

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

#[tokio::test]
async fn mcp_manager_call_tool_signature_unchanged() {
    let module_file = echo_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("wasm MCP server should connect");

    let output = manager
        .call_tool(
            "github",
            "echo",
            json!({ "query": "simulacra" }),
            &capability_with_mcp_tools(&["mcp:github:echo"]),
        )
        .await
        .expect("call_tool signature should still support direct await usage");

    assert_eq!(output["echoed"]["query"], json!("simulacra"));
}

#[test]
fn gen_ai_tool_message_event_for_wasm_mcp_carries_simulacra_tool_source_attribute() {
    let module_file = echo_component_fixture();

    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager
                .connect_wasm_module("github", module)
                .await
                .expect("wasm MCP server should connect");
            manager
                .call_tool(
                    "github",
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability_with_mcp_tools(&["mcp:github:echo"]),
                )
                .await
        })
    });

    assert!(
        events.iter().any(|event| {
            field_matches(&event.fields, "event", "gen_ai.tool.message")
                && field_matches(&event.fields, "simulacra.tool.source", "mcp:github")
        }),
        "WASM MCP calls should preserve simulacra.tool.source on gen_ai.tool.message events"
    );
}

#[test]
fn simulacra_mcp_calls_counter_increments_for_wasm_mcp_with_server_and_tool_labels() {
    let module_file = echo_component_fixture();

    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager
                .connect_wasm_module("github", module)
                .await
                .expect("wasm MCP server should connect");
            manager
                .call_tool(
                    "github",
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability_with_mcp_tools(&["mcp:github:echo"]),
                )
                .await
        })
    });

    assert!(
        events.iter().any(|event| {
            field_matches(&event.fields, "counter.simulacra.mcp.calls", "1")
                && field_matches(&event.fields, "server", "github")
                && field_matches(&event.fields, "tool", "echo")
        }),
        "WASM MCP calls should increment simulacra.mcp.calls with server/tool labels"
    );
}

#[tokio::test]
async fn http_sse_mcp_servers_continue_to_work_unchanged() {
    let server = spawn_json_rpc_test_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "input_schema": { "type": "object" }
                }]
            }
        })
        .to_string(),
        &json!({
            "jsonrpc": "2.0",
            "result": { "echoed": { "query": "simulacra" } }
        })
        .to_string(),
    );

    let mut manager = McpManager::new();
    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("HTTP MCP server should register");

    let deadline = Instant::now() + Duration::from_secs(2);
    let tools = loop {
        let tools = manager.list_tools().await;
        if !tools.is_empty() || Instant::now() >= deadline {
            break tools;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(tools.len(), 1, "HTTP MCP servers should still list tools");
    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "query": "simulacra" }),
            &capability_with_mcp_tools(&["mcp:*:echo"]),
        )
        .await
        .expect("HTTP MCP servers should still handle call_tool");
    assert_eq!(output["echoed"]["query"], json!("simulacra"));

    let module_file = echo_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("adding wasm MCP should not break existing HTTP/SSE flows");
}

#[test]
fn simulacra_mcp_handshake_span_carries_transport_mode_wasm() {
    let module_file = echo_component_fixture();

    let (_result, spans, _events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager.connect_wasm_module("github", module).await
        })
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "simulacra_mcp_handshake"
                && field_matches(&span.fields, "simulacra.mcp.transport_mode", "wasm")
        }),
        "WASM MCP handshake spans should record transport_mode=wasm"
    );
}

#[test]
fn simulacra_mcp_handshake_span_carries_module_id() {
    let module_file = echo_component_fixture();

    let (_result, spans, _events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager.connect_wasm_module("github", module).await
        })
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "simulacra_mcp_handshake"
                && span.fields.contains_key("simulacra.mcp.module_id")
        }),
        "WASM MCP handshake spans should record simulacra.mcp.module_id"
    );
}

#[test]
fn simulacra_mcp_tool_call_span_carries_simulacra_wasm_fuel_consumed() {
    let module_file = echo_component_fixture();

    let (_result, spans, _events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager
                .connect_wasm_module("github", module)
                .await
                .expect("wasm MCP server should connect");
            manager
                .call_tool(
                    "github",
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability_with_mcp_tools(&["mcp:github:echo"]),
                )
                .await
        })
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "simulacra_mcp_tool_call"
                && span.fields.contains_key("simulacra.wasm.fuel_consumed")
        }),
        "WASM MCP tool call spans should record consumed fuel"
    );
}

#[test]
fn simulacra_wasm_fuel_consumed_histogram_records_per_call_with_module_and_tool_labels() {
    let module_file = echo_component_fixture();

    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager
                .connect_wasm_module("github", module)
                .await
                .expect("wasm MCP server should connect");
            manager
                .call_tool(
                    "github",
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability_with_mcp_tools(&["mcp:github:echo"]),
                )
                .await
        })
    });

    // After the OTel meter switch, fuel-consumed lands as a real
    // `simulacra.wasm.fuel_consumed` histogram via the meter API (verified
    // end-to-end against Aniani). Locally we assert the mirror
    // structured log carries the labels and a non-zero value so the
    // recording site is exercised by every WASM MCP tool call.
    assert!(
        events.iter().any(|event| {
            event
                .fields
                .get("message")
                .map(|m| m.contains("WASM MCP fuel consumed"))
                .unwrap_or(false)
                && field_matches(&event.fields, "module", "github")
                && field_matches(&event.fields, "tool", "echo")
                && event
                    .fields
                    .get("value")
                    .map(|v| v.parse::<u64>().ok().map(|n| n > 0).unwrap_or(false))
                    .unwrap_or(false)
        }),
        "fuel-consumed log mirror should carry module/tool labels and a non-zero value for each call; got events: {events:?}"
    );
}

#[test]
fn simulacra_mcp_http_fetch_span_records_method_url_host_response_status() {
    // Drives `wasm_mcp_fetch` against an unreachable URL with a permissive
    // allowlist; the transport fails but the span still records its keys
    // (status_code = 0 for failure paths).
    let (_result, spans, _events) = capture_traces(|| {
        run_async(async {
            wasm_mcp_fetch(
                "github",
                fetch_request_to("http://127.0.0.1:1/probe"),
                &["127.0.0.1:1".to_string()],
                None,
                None,
                &simulacra_types::AgentId(String::new()),
            )
            .await
        })
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "simulacra_mcp_http_fetch"
                && span.fields.contains_key("http.method")
                && span.fields.contains_key("http.url.host")
                && span.fields.contains_key("http.response.status_code")
        }),
        "outbound simulacra:http/fetch spans should record method, host, and response status"
    );
}

#[test]
fn simulacra_mcp_http_denied_counter_increments_on_capability_or_hook_denial() {
    // Drives `wasm_mcp_fetch` with an empty allowlist so the request is
    // denied before any network IO; the counter increments at that gate.
    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            wasm_mcp_fetch(
                "github",
                fetch_request_to("https://api.github.com/repos"),
                &[],
                None,
                None,
                &simulacra_types::AgentId(String::new()),
            )
            .await
        })
    });

    assert!(
        events.iter().any(|event| {
            field_matches(&event.fields, "counter.simulacra.mcp.http.denied", "1")
                && field_matches(&event.fields, "server", "github")
        }),
        "capability or hook denials should increment simulacra.mcp.http.denied"
    );
}

#[test]
fn tracing_warn_emitted_on_hook_denial_inside_simulacra_http_fetch() {
    // Drives `wasm_mcp_fetch` with a denying `Phase::Before` hook and a
    // permissive allowlist so the request reaches the hook layer.
    let hooks = deny_before_pipeline();
    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            wasm_mcp_fetch(
                "github",
                fetch_request_to("https://api.github.com/repos"),
                &["api.github.com:443".to_string()],
                Some(&hooks),
                None,
                &simulacra_types::AgentId(String::new()),
            )
            .await
        })
    });

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("hook denial"))
        }),
        "hook denials inside simulacra:http/fetch should emit WARN logs"
    );
}

#[test]
fn tracing_error_emitted_on_wasm_trap_during_call_tool() {
    // Uses the trap-mcp fixture whose `trap` tool calls `unreachable!()` —
    // wasmtime surfaces this as a real WASM trap, which the dispatch path
    // logs at ERROR.
    let module_file = trap_component_fixture();

    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager
                .connect_wasm_module("github", module)
                .await
                .expect("wasm MCP server should connect");
            manager
                .call_tool(
                    "github",
                    "trap",
                    json!({}),
                    &capability_with_mcp_tools(&["mcp:github:trap"]),
                )
                .await
        })
    });

    assert!(
        events.iter().any(|event| {
            event.level == "ERROR"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("WASM trap"))
        }),
        "WASM traps during call_tool should emit ERROR logs"
    );
}
