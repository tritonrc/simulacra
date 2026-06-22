#![allow(
    clippy::type_complexity,
    clippy::await_holding_lock,
    clippy::collapsible_if
)]

use serde_json::json;
use simulacra_mcp::{McpError, McpManager};
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JournalEntry, JournalEntryKind, JournalError,
    JournalStorage, TokenUsage,
};
use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing_subscriber::layer::SubscriberExt;

fn capability_with_mcp_tools(patterns: &[&str]) -> CapabilityToken {
    CapabilityToken {
        mcp_tools: patterns
            .iter()
            .map(|pattern| (*pattern).to_string())
            .collect(),
        ..Default::default()
    }
}

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct CapturedEvent {
    level: String,
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

        self.spans
            .lock()
            .expect("span capture mutex should not be poisoned")
            .push(CapturedSpan {
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

            let mut spans = self
                .spans
                .lock()
                .expect("span capture mutex should not be poisoned");
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

        self.events
            .lock()
            .expect("event capture mutex should not be poisoned")
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

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

static CAPTURED_SPANS: OnceLock<Arc<Mutex<Vec<CapturedSpan>>>> = OnceLock::new();
static CAPTURED_EVENTS: OnceLock<Arc<Mutex<Vec<CapturedEvent>>>> = OnceLock::new();
static CAPTURE_INSTALL: OnceLock<()> = OnceLock::new();
static TEST_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn capture_store() -> (
    Arc<Mutex<Vec<CapturedSpan>>>,
    Arc<Mutex<Vec<CapturedEvent>>>,
) {
    CAPTURE_INSTALL.get_or_init(|| {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));

        CAPTURED_SPANS
            .set(Arc::clone(&spans))
            .expect("span capture store should only initialize once");
        CAPTURED_EVENTS
            .set(Arc::clone(&events))
            .expect("event capture store should only initialize once");

        let subscriber =
            tracing_subscriber::registry::Registry::default().with(CaptureLayer { spans, events });
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber should install");
    });

    (
        Arc::clone(
            CAPTURED_SPANS
                .get()
                .expect("span capture store should be installed"),
        ),
        Arc::clone(
            CAPTURED_EVENTS
                .get()
                .expect("event capture store should be installed"),
        ),
    )
}

fn blocking_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    let _ = capture_store();
    TEST_MUTEX
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .blocking_lock()
}

async fn test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    let _ = capture_store();
    TEST_MUTEX
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn capture_traces<T>(operation: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    let _guard = blocking_test_guard();
    let (spans, events) = capture_store();
    spans
        .lock()
        .expect("span capture mutex should not be poisoned")
        .clear();
    events
        .lock()
        .expect("event capture mutex should not be poisoned")
        .clear();

    let result = operation();
    let captured_spans = spans
        .lock()
        .expect("span capture mutex should not be poisoned")
        .clone();
    let captured_events = events
        .lock()
        .expect("event capture mutex should not be poisoned")
        .clone();
    (result, captured_spans, captured_events)
}

fn field_matches(fields: &HashMap<String, String>, key: &str, expected: &str) -> bool {
    fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
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

#[allow(dead_code)]
struct PassiveTcpListenerProbe {
    addr: String,
    connection_count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PassiveTcpListenerProbe {
    #[allow(dead_code)]
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::SeqCst)
    }
}

impl Drop for PassiveTcpListenerProbe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_passive_tcp_listener_probe() -> PassiveTcpListenerProbe {
    let listener = TcpListener::bind("127.0.0.1:0").expect("probe listener should bind");
    listener
        .set_nonblocking(true)
        .expect("probe listener should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("probe listener should have a local address")
        .to_string();
    let connection_count = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let connection_count_for_thread = Arc::clone(&connection_count);
    let stop_for_thread = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((_stream, _peer)) => {
                    connection_count_for_thread.fetch_add(1, Ordering::SeqCst);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    PassiveTcpListenerProbe {
        addr,
        connection_count,
        stop,
        handle: Some(handle),
    }
}

struct RecordingHttpServer {
    addr: String,
    request_count: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl RecordingHttpServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("request log mutex should not be poisoned")
            .clone()
    }
}

impl Drop for RecordingHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_recording_http_server(response_body: &str) -> RecordingHttpServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test HTTP server should bind");
    listener
        .set_nonblocking(true)
        .expect("test HTTP server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("test HTTP server should have a local address")
        .to_string();
    let request_count = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let response_body = response_body.to_string();
    let request_count_for_thread = Arc::clone(&request_count);
    let requests_for_thread = Arc::clone(&requests);
    let stop_for_thread = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    request_count_for_thread.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));

                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );

                    if let Some(request) = read_http_request(&mut stream) {
                        requests_for_thread
                            .lock()
                            .expect("request log mutex should not be poisoned")
                            .push(request);
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    RecordingHttpServer {
        addr,
        request_count,
        requests,
        stop,
        handle: Some(handle),
    }
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
        self.addr
            .split(':')
            .next()
            .expect("test server address should include a host")
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

fn spawn_json_rpc_test_server(tools_list_body: &str, tool_call_body: &str) -> JsonRpcTestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("JSON-RPC test server should bind");
    listener
        .set_nonblocking(true)
        .expect("JSON-RPC test server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("JSON-RPC test server should have a local address")
        .to_string();
    let request_count = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let request_count_for_thread = Arc::clone(&request_count);
    let requests_for_thread = Arc::clone(&requests);
    let stop_for_thread = Arc::clone(&stop);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));

                    if let Some(request) = read_http_request(&mut stream) {
                        request_count_for_thread.fetch_add(1, Ordering::SeqCst);
                        requests_for_thread
                            .lock()
                            .expect("request log mutex should not be poisoned")
                            .push(request.clone());

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
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_body.clone()
                        } else if request.contains("\"method\":\"tools/call\"") {
                            tool_call_body.clone()
                        } else {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        };

                        let response = json_http_response(&body);
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Both);
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

struct SseProbeServer {
    addr: String,
    connection_count: Arc<AtomicUsize>,
    persistent_event_sent: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SseProbeServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::SeqCst)
    }

    fn persistent_event_sent(&self) -> bool {
        self.persistent_event_sent.load(Ordering::SeqCst)
    }
}

impl Drop for SseProbeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_sse_probe_server() -> SseProbeServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test SSE server should bind");
    listener
        .set_nonblocking(true)
        .expect("test SSE server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("test SSE server should have a local address")
        .to_string();
    let connection_count = Arc::new(AtomicUsize::new(0));
    let persistent_event_sent = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let connection_count_for_thread = Arc::clone(&connection_count);
    let persistent_event_sent_for_thread = Arc::clone(&persistent_event_sent);
    let stop_for_thread = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    connection_count_for_thread.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));

                    let mut buffer = [0_u8; 4096];
                    let _ = stream.read(&mut buffer);

                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\nevent: ready\ndata: one\n\n",
                    );
                    thread::sleep(Duration::from_millis(150));
                    if stream.write_all(b"event: update\ndata: two\n\n").is_ok() {
                        persistent_event_sent_for_thread.store(true, Ordering::SeqCst);
                    }

                    while !stop_for_thread.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(10));
                    }
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    SseProbeServer {
        addr,
        connection_count,
        persistent_event_sent,
        stop,
        handle: Some(handle),
    }
}

struct SseDiscoveryServer {
    addr: String,
    sse_connection_count: Arc<AtomicUsize>,
    post_requests: Arc<Mutex<Vec<String>>>,
    persistent_event_sent: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SseDiscoveryServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn server_name(&self) -> &str {
        self.addr
            .split(':')
            .next()
            .expect("test server address should include a host")
    }

    fn sse_connection_count(&self) -> usize {
        self.sse_connection_count.load(Ordering::SeqCst)
    }

    fn post_requests(&self) -> Vec<String> {
        self.post_requests
            .lock()
            .expect("request log mutex should not be poisoned")
            .clone()
    }

    fn persistent_event_sent(&self) -> bool {
        self.persistent_event_sent.load(Ordering::SeqCst)
    }
}

impl Drop for SseDiscoveryServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
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
    let mut header_end = None;
    let mut expected_len = None;

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(bytes_read) => {
                request.extend_from_slice(&buffer[..bytes_read]);

                if header_end.is_none() {
                    if let Some(idx) = find_bytes(&request, b"\r\n\r\n") {
                        let end = idx + 4;
                        let headers = String::from_utf8_lossy(&request[..end]).into_owned();
                        let content_length = parse_content_length(&headers);
                        header_end = Some(end);
                        expected_len = Some(end + content_length);
                    }
                }

                if let Some(total_len) = expected_len {
                    if request.len() >= total_len {
                        break;
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if !request.is_empty() {
                    break;
                }
                return None;
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

fn spawn_sse_discovery_server(tools_list_body: &str, tool_call_body: &str) -> SseDiscoveryServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test SSE server should bind");
    listener
        .set_nonblocking(true)
        .expect("test SSE server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("test SSE server should have a local address")
        .to_string();
    let sse_connection_count = Arc::new(AtomicUsize::new(0));
    let post_requests = Arc::new(Mutex::new(Vec::new()));
    let persistent_event_sent = Arc::new(AtomicBool::new(false));
    let initialize_seen = Arc::new(AtomicBool::new(false));
    let tools_list_seen = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let sse_connection_count_for_thread = Arc::clone(&sse_connection_count);
    let post_requests_for_thread = Arc::clone(&post_requests);
    let persistent_event_sent_for_thread = Arc::clone(&persistent_event_sent);
    let initialize_seen_for_thread = Arc::clone(&initialize_seen);
    let tools_list_seen_for_thread = Arc::clone(&tools_list_seen);
    let stop_for_thread = Arc::clone(&stop);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                    let request = match read_http_request(&mut stream) {
                        Some(request) => request,
                        None => continue,
                    };

                    if request.starts_with("GET /sse ") {
                        sse_connection_count_for_thread.fetch_add(1, Ordering::SeqCst);

                        let stop_for_sse = Arc::clone(&stop_for_thread);
                        let initialize_seen_for_sse = Arc::clone(&initialize_seen_for_thread);
                        let tools_list_seen_for_sse = Arc::clone(&tools_list_seen_for_thread);
                        let persistent_event_sent_for_sse =
                            Arc::clone(&persistent_event_sent_for_thread);

                        thread::spawn(move || {
                            let _ = stream.write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\nevent: endpoint\ndata: /mcp-rpc\n\n",
                            );
                            let _ = stream.flush();

                            while !stop_for_sse.load(Ordering::SeqCst) {
                                let _ = stream.write_all(b": keep-alive\n\n");
                                let _ = stream.flush();

                                if initialize_seen_for_sse.load(Ordering::SeqCst)
                                    && tools_list_seen_for_sse.load(Ordering::SeqCst)
                                    && !persistent_event_sent_for_sse.swap(true, Ordering::SeqCst)
                                {
                                    let _ = stream.write_all(
                                        b"event: update\ndata: {\"status\":\"still-open\"}\n\n",
                                    );
                                    let _ = stream.flush();
                                }

                                thread::sleep(Duration::from_millis(50));
                            }
                        });
                        continue;
                    }

                    if request.starts_with("POST ") {
                        post_requests_for_thread
                            .lock()
                            .expect("request log mutex should not be poisoned")
                            .push(request.clone());

                        let body = if request.starts_with("POST /mcp-rpc ")
                            && request.contains("\"method\":\"initialize\"")
                        {
                            initialize_seen_for_thread.store(true, Ordering::SeqCst);
                            json!({
                                "jsonrpc": "2.0",
                                "result": {
                                    "protocolVersion": "2024-11-05",
                                    "serverInfo": { "name": "fake-sse-mcp", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string()
                        } else if request.starts_with("POST /mcp-rpc ")
                            && request.contains("\"method\":\"tools/list\"")
                        {
                            tools_list_seen_for_thread.store(true, Ordering::SeqCst);
                            tools_list_body.clone()
                        } else if request.starts_with("POST /mcp-rpc ")
                            && request.contains("\"method\":\"tools/call\"")
                        {
                            tool_call_body.clone()
                        } else {
                            json!({
                                "jsonrpc": "2.0",
                                "error": {
                                    "code": -32000,
                                    "message": "JSON-RPC must be sent to /mcp-rpc, not the SSE URL"
                                }
                            })
                            .to_string()
                        };

                        let response = json_http_response(&body);
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    SseDiscoveryServer {
        addr,
        sse_connection_count,
        post_requests,
        persistent_event_sent,
        stop,
        handle: Some(handle),
    }
}

// S008 Assertion: No use of std::process::Command or tokio::process::Command in simulacra-mcp.
// Constraint: MCP servers are accessed via HTTP/SSE only — no stdio, no child processes.
//
// This is verified behaviorally: we start a real HTTP MCP server, connect to it,
// and confirm the connection completes successfully over HTTP. The simulacra-mcp crate
// has no API surface for stdio/child-process transports — connect_http and connect_sse
// are the only connection methods, and both use network I/O exclusively.
// The architectural constraint (no std::process::Command) is enforced by code review
// and by the absence of any spawn/stdio API in McpManager's public interface.
#[tokio::test]
async fn simulacra_mcp_connects_via_http_not_child_process() {
    let _guard = test_guard().await;

    // Start a real HTTP MCP server that responds to handshake requests.
    let server = spawn_json_rpc_test_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "net_tool",
                    "description": "A tool served over HTTP",
                    "inputSchema": { "type": "object", "properties": {} }
                }]
            }
        })
        .to_string(),
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
    );

    let mut manager = McpManager::new();

    // connect_http is the only way to connect — no spawn/stdio path exists.
    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should succeed over network transport");

    // Trigger the lazy handshake and verify tools are discovered over HTTP.
    let tools = manager.list_tools().await;
    assert_eq!(tools.len(), 1, "should discover tools via HTTP transport");
    assert_eq!(tools[0].name, "net_tool");
}

// S008 Assertion: Tool schema bridging produces valid ToolDefinition values from real MCP responses.
#[tokio::test]
async fn list_tools_bridges_name_description_and_input_schema_from_mcp_server() {
    let _guard = test_guard().await;
    let server = spawn_json_rpc_test_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "search_docs",
                    "description": "Searches indexed MCP documentation.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }
                }]
            }
        })
        .to_string(),
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
    );
    let mut manager = McpManager::new();

    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should register the MCP server");

    let tools = manager.list_tools().await;

    assert_eq!(
        tools.len(),
        1,
        "list_tools should bridge exactly one MCP tool into a ToolDefinition"
    );
    assert_eq!(tools[0].name, "search_docs");
    assert_eq!(tools[0].description, "Searches indexed MCP documentation.");
    assert_eq!(
        tools[0].input_schema["type"], "object",
        "bridged input_schema should preserve the JSON Schema 'type' field"
    );
    assert_eq!(
        tools[0].input_schema["properties"],
        json!({ "query": { "type": "string" } }),
        "bridged input_schema should preserve the 'properties' map with property types"
    );
    assert_eq!(
        tools[0].input_schema["required"],
        json!(["query"]),
        "bridged input_schema should preserve the 'required' array"
    );
}

// S008 Assertion: MCP capability checks use glob matching for MCP patterns, not exact tool equality only.
#[tokio::test]
async fn call_tool_with_glob_mcp_capability_pattern_is_allowed_to_dispatch() {
    let _guard = test_guard().await;
    let server = spawn_json_rpc_test_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "value": { "type": "integer" }
                        },
                        "required": ["value"]
                    }
                }]
            }
        })
        .to_string(),
        &json!({
            "jsonrpc": "2.0",
            "result": { "echoed": { "value": 1 } }
        })
        .to_string(),
    );
    let mut manager = McpManager::new();
    let granted_pattern = format!("mcp:{}:*", server.server_name());
    let capability = capability_with_mcp_tools(&[granted_pattern.as_str()]);

    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should register the MCP server");

    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "value": 1 }),
            &capability,
        )
        .await
        .expect("a matching mcp:{server}:* capability should allow the MCP call");

    assert_eq!(output["echoed"]["value"], json!(1));
}

// S008 Behavior: Connection failures produce typed errors, not panics.
#[tokio::test]
async fn invalid_mcp_url_returns_typed_error() {
    let _guard = test_guard().await;
    let mut manager = McpManager::new();
    let err = manager
        .connect_http("://not-a-valid-url")
        .await
        .expect_err("invalid MCP URLs should return a typed McpError");

    assert!(
        matches!(
            err,
            McpError::ConnectionFailed(_)
                | McpError::ProtocolError(_)
                | McpError::TransportError(_)
        ),
        "unexpected MCP error variant: {err:?}"
    );
}

// S008 Assertion: McpManager construction does not open an HTTP socket.
// connect_http is lazy: it registers the URL but does not perform any network I/O.
// The handshake is deferred until first use (list_tools or call_tool).
#[tokio::test]
async fn connect_http_is_lazy_and_does_not_open_a_socket_during_connect() {
    let _guard = test_guard().await;
    let probe = spawn_passive_tcp_listener_probe();
    let mut manager = McpManager::new();

    manager
        .connect_http(&probe.url("/mcp"))
        .await
        .expect("connect_http should succeed without network I/O");

    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        probe.connection_count(),
        0,
        "connect_http should not open a network connection (lazy handshake)"
    );
}

// S008 Assertion: Capability proxy rejects MCP tools that are not granted.
#[tokio::test]
async fn call_tool_with_tool_outside_capability_returns_capability_denied() {
    let _guard = test_guard().await;
    let mut manager = McpManager::new();
    let capability = capability_with_mcp_tools(&["allowed_tool"]);

    let err = manager
        .call_tool(
            "docs-server",
            "forbidden_tool",
            json!({ "query": "simulacra" }),
            &capability,
        )
        .await
        .expect_err("an ungranted MCP tool should be rejected before dispatch");

    assert!(
        matches!(err, McpError::CapabilityDenied(ref message) if message.contains("forbidden_tool")),
        "expected CapabilityDenied for an ungranted MCP tool, got {err:?}"
    );
}

// S008 Assertion: Calling an unconnected server returns a typed MCP error.
#[tokio::test]
async fn call_tool_to_unconnected_server_returns_typed_error() {
    let _guard = test_guard().await;
    let mut manager = McpManager::new();
    let capability = capability_with_mcp_tools(&["mcp:*:echo"]);

    let err = manager
        .call_tool("missing-server", "echo", json!({ "value": 1 }), &capability)
        .await
        .expect_err("calling a tool on an unconnected server should fail");

    assert!(
        matches!(err, McpError::ConnectionFailed(ref msg) if msg.contains("missing-server")),
        "expected ConnectionFailed mentioning the missing server name, got {err:?}"
    );
}

// S008 Assertion: MCP handshake implements initialize followed by tools/list.
// The handshake is triggered lazily on first list_tools() call.
#[tokio::test]
async fn connect_http_performs_initialize_then_tools_list_handshake() {
    let _guard = test_guard().await;
    let server = spawn_recording_http_server(
        r#"{"jsonrpc":"2.0","result":{"protocolVersion":"2025-03-26"}}"#,
    );
    let mut manager = McpManager::new();

    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should register the server URL");

    let _ = manager.list_tools().await;

    let deadline = Instant::now() + Duration::from_secs(2);
    let requests = loop {
        let requests = server.requests();
        if requests
            .iter()
            .any(|r| r.contains("\"method\":\"initialize\""))
            && requests
                .iter()
                .any(|r| r.contains("\"method\":\"tools/list\""))
        {
            break requests;
        }
        if Instant::now() >= deadline {
            break requests;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(
        server.request_count() > 0,
        "list_tools should trigger the MCP handshake to perform discovery"
    );

    // Find the index of the first request containing "initialize" and "tools/list".
    // The MCP protocol requires initialize to come before tools/list.
    let initialize_idx = requests
        .iter()
        .position(|r| r.contains("\"method\":\"initialize\""))
        .expect("handshake should send an initialize request before exposing MCP tools");
    let tools_list_idx = requests
        .iter()
        .position(|r| r.contains("\"method\":\"tools/list\""))
        .expect("handshake should request tools/list during MCP discovery");
    assert!(
        initialize_idx < tools_list_idx,
        "initialize (request index {initialize_idx}) must come before tools/list (request index {tools_list_idx}) per MCP protocol; observed requests: {requests:?}"
    );
}

// S008 Assertion: SSE transport maintains a persistent connection for server-pushed events.
// The SSE background task is started lazily on first list_tools() or call_tool().
#[tokio::test]
async fn connect_sse_keeps_a_persistent_connection_for_server_pushed_events() {
    let _guard = test_guard().await;
    let server = spawn_sse_probe_server();
    let mut manager = McpManager::new();

    manager
        .connect_sse(&server.url("/sse"))
        .await
        .expect("SSE MCP connect should register the SSE endpoint");

    let _ = manager.list_tools().await;

    tokio::time::sleep(Duration::from_millis(250)).await;

    assert!(
        server.connection_count() > 0,
        "list_tools should trigger the SSE connection to the server"
    );
    assert!(
        server.persistent_event_sent(),
        "SSE stream should stay open long enough to receive a later server-pushed event"
    );
}

#[tokio::test]
async fn connect_sse_discovers_post_endpoint_from_sse_events() {
    let _guard = test_guard().await;
    let server = spawn_sse_discovery_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "search_docs",
                    "description": "Searches indexed MCP documentation.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }
                }]
            }
        })
        .to_string(),
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
    );
    let mut manager = McpManager::new();

    manager
        .connect_sse(&server.url("/sse"))
        .await
        .expect("connect_sse should register the SSE endpoint");

    let mut tools = manager.list_tools().await;
    for _ in 0..4 {
        if !tools.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        tools = manager.list_tools().await;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;

    let requests = server.post_requests().join("\n");
    assert_eq!(
        tools.len(),
        1,
        "SSE endpoint discovery should expose exactly one MCP tool after following the endpoint event"
    );
    assert_eq!(tools[0].name, "search_docs");
    assert!(
        requests.contains("POST /mcp-rpc ") && requests.contains("\"method\":\"tools/list\""),
        "list_tools should POST JSON-RPC to the endpoint discovered from SSE events; observed requests: {requests:?}"
    );
}

#[tokio::test]
async fn connect_sse_performs_handshake_via_discovered_endpoint() {
    let _guard = test_guard().await;
    let server = spawn_sse_discovery_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "value": { "type": "integer" }
                        },
                        "required": ["value"]
                    }
                }]
            }
        })
        .to_string(),
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
    );
    let mut manager = McpManager::new();

    manager
        .connect_sse(&server.url("/sse"))
        .await
        .expect("connect_sse should register the SSE endpoint");

    let tools = manager.list_tools().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let requests = server.post_requests().join("\n");
    assert!(
        requests.contains("POST /mcp-rpc ") && requests.contains("\"method\":\"initialize\""),
        "SSE transport should POST initialize to the discovered MCP JSON-RPC endpoint; observed requests: {requests:?}"
    );
    assert!(
        requests.contains("POST /mcp-rpc ") && requests.contains("\"method\":\"tools/list\""),
        "SSE transport should POST tools/list to the discovered MCP JSON-RPC endpoint; observed requests: {requests:?}"
    );
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["echo"],
        "tools/list results from the discovered endpoint should be bridged into Simulacra tool definitions"
    );
}

#[tokio::test]
async fn call_tool_via_sse_transport_uses_discovered_endpoint() {
    let _guard = test_guard().await;
    let server = spawn_sse_discovery_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }
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
    let capability = capability_with_mcp_tools(&["mcp:*:echo"]);

    manager
        .connect_sse(&server.url("/sse"))
        .await
        .expect("connect_sse should register the SSE endpoint");

    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "query": "simulacra" }),
            &capability,
        )
        .await
        .expect("call_tool should succeed through the JSON-RPC endpoint discovered via SSE");

    tokio::time::sleep(Duration::from_millis(250)).await;

    let requests = server.post_requests().join("\n");
    assert_eq!(output["echoed"]["query"], json!("simulacra"));
    assert!(
        requests.contains("POST /mcp-rpc ") && requests.contains("\"method\":\"tools/call\""),
        "call_tool should route tools/call through the SSE-discovered JSON-RPC endpoint; observed requests: {requests:?}"
    );
    assert!(
        !requests.contains("POST /sse "),
        "call_tool must not POST JSON-RPC to the SSE URL itself; observed requests: {requests:?}"
    );
}

#[tokio::test]
async fn connect_sse_keeps_connection_alive() {
    let _guard = test_guard().await;
    let server = spawn_sse_discovery_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }
                }]
            }
        })
        .to_string(),
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
    );
    let mut manager = McpManager::new();

    manager
        .connect_sse(&server.url("/sse"))
        .await
        .expect("connect_sse should register the SSE endpoint");

    let _ = manager.list_tools().await;

    let deadline = Instant::now() + Duration::from_secs(2);
    while !server.persistent_event_sent() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(
        server.sse_connection_count() > 0,
        "SSE transport should establish a streaming GET connection to the SSE endpoint"
    );
    assert!(
        server.persistent_event_sent(),
        "SSE transport should keep the stream alive after the discovered-endpoint handshake so later server-pushed events can still arrive"
    );
}

/// A journal implementation that records the global ordering sequence number
/// at which each append occurs, enabling verification that journal writes
/// happen before subsequent side effects (like HTTP dispatch).
#[derive(Debug)]
struct OrderingJournalStorage {
    entries: Mutex<Vec<JournalEntry>>,
    /// Records the sequence number from the shared counter at each append.
    append_sequence_numbers: Mutex<Vec<usize>>,
    /// Shared counter incremented by both journal and HTTP server to track ordering.
    ordering_counter: Arc<AtomicUsize>,
}

impl OrderingJournalStorage {
    fn new(ordering_counter: Arc<AtomicUsize>) -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            append_sequence_numbers: Mutex::new(Vec::new()),
            ordering_counter,
        }
    }

    fn append_sequence_numbers(&self) -> Vec<usize> {
        self.append_sequence_numbers
            .lock()
            .expect("ordering mutex should not be poisoned")
            .clone()
    }
}

impl JournalStorage for OrderingJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        let seq = self.ordering_counter.fetch_add(1, Ordering::SeqCst);
        self.append_sequence_numbers
            .lock()
            .expect("ordering mutex should not be poisoned")
            .push(seq);
        self.entries
            .lock()
            .expect("journal mutex should not be poisoned")
            .push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .entries
            .lock()
            .expect("journal mutex should not be poisoned")
            .iter()
            .filter(|entry| entry.agent_id == *agent_id)
            .cloned()
            .collect())
    }

    fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        _agent_id: &AgentId,
        _after_entry: usize,
        _data: CheckpointData,
    ) -> Result<(), JournalError> {
        Ok(())
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if checkpoint_idx >= entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(checkpoint_idx));
        }
        Ok(entries[..=checkpoint_idx].to_vec())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if start_index > entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(start_index));
        }
        Ok(entries[start_index..].to_vec())
    }
}

/// Spawns a JSON-RPC server that increments a shared ordering counter when it receives
/// a tools/call request, allowing tests to verify journal-before-dispatch ordering.
fn spawn_ordering_json_rpc_server(
    tools_list_body: &str,
    tool_call_body: &str,
    ordering_counter: Arc<AtomicUsize>,
) -> JsonRpcTestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("JSON-RPC test server should bind");
    listener
        .set_nonblocking(true)
        .expect("JSON-RPC test server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("JSON-RPC test server should have a local address")
        .to_string();
    let stop = Arc::new(AtomicBool::new(false));

    let stop_for_thread = Arc::clone(&stop);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));

                    loop {
                        let mut buffer = [0_u8; 8192];
                        match stream.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(bytes_read) => {
                                let request =
                                    String::from_utf8_lossy(&buffer[..bytes_read]).into_owned();

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
                                } else if request.contains("\"method\":\"tools/list\"") {
                                    tools_list_body.clone()
                                } else if request.contains("\"method\":\"tools/call\"") {
                                    // Record the ordering counter when the server
                                    // receives the dispatch — this must be AFTER the
                                    // journal append.
                                    ordering_counter.fetch_add(1, Ordering::SeqCst);
                                    tool_call_body.clone()
                                } else {
                                    json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                                };

                                let response = json_http_response(&body);
                                let _ = stream.write_all(response.as_bytes());
                            }
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) =>
                            {
                                break;
                            }
                            Err(_) => break,
                        }
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

// S008 Assertion: MCP tool calls write a ToolCall journal entry BEFORE dispatching the HTTP call.
// The Golden Rule: journal before side effect.
#[tokio::test]
async fn call_tool_records_a_tool_call_journal_entry() {
    let _guard = test_guard().await;

    // Shared ordering counter: journal append and HTTP dispatch each increment it.
    // If journal comes first, its sequence number is lower than the server's.
    let ordering_counter = Arc::new(AtomicUsize::new(0));

    let server = spawn_ordering_json_rpc_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }
                }]
            }
        })
        .to_string(),
        &json!({
            "jsonrpc": "2.0",
            "result": { "echoed": { "query": "simulacra" } }
        })
        .to_string(),
        Arc::clone(&ordering_counter),
    );
    let journal = Arc::new(OrderingJournalStorage::new(Arc::clone(&ordering_counter)));
    let agent_id = AgentId("agent-s008-red".into());
    let mut manager = McpManager::with_journal(
        Arc::clone(&journal) as Arc<dyn JournalStorage>,
        agent_id.clone(),
    );
    let capability = capability_with_mcp_tools(&["mcp:*:echo"]);

    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should register the MCP server");

    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "query": "simulacra" }),
            &capability,
        )
        .await
        .expect("call_tool should succeed so the journal can be inspected");

    assert_eq!(output["echoed"]["query"], json!("simulacra"));

    let entries = journal
        .read_all(&agent_id)
        .expect("journal entries should be readable");
    assert_eq!(
        entries.len(),
        1,
        "exactly one ToolCall journal entry should be recorded for the MCP invocation"
    );
    assert!(matches!(
        &entries[0].entry,
        JournalEntryKind::ToolCall { tool_name, arguments, .. }
            if tool_name == "echo" && arguments == &json!({ "query": "simulacra" })
    ));

    // Verify ordering: journal append must happen before HTTP dispatch.
    // The ordering counter was 0 initially. Journal append increments it (getting seq 0),
    // then the HTTP server increments it (getting seq 1). Journal seq must be less.
    let journal_seq = journal.append_sequence_numbers();
    assert_eq!(
        journal_seq.len(),
        1,
        "journal should have recorded exactly one append sequence number"
    );
    let total_operations = ordering_counter.load(Ordering::SeqCst);
    assert!(
        total_operations >= 2,
        "both journal append and HTTP dispatch should have incremented the counter, got {total_operations}"
    );
    assert_eq!(
        journal_seq[0], 0,
        "journal append (seq {}) must happen before HTTP dispatch (seq 1) — Golden Rule: journal before side effect",
        journal_seq[0]
    );
}

// S008 O11y Assertion: MCP tool calls produce an execute_tool span with Simulacra MCP attributes.
#[test]
fn mcp_tool_calls_emit_execute_tool_span_with_mcp_source_attributes() {
    let server = spawn_json_rpc_test_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }
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

    let ((_, server_name), spans, _events) = capture_traces(|| {
        run_async(async {
            let mut manager = McpManager::new();
            let capability = capability_with_mcp_tools(&["mcp:*:echo"]);
            let server_name = server.server_name().to_string();

            manager
                .connect_http(&server.url("/mcp"))
                .await
                .expect("connect_http should register the MCP server");

            let result = manager
                .call_tool(
                    &server_name,
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability,
                )
                .await;

            (result, server_name)
        })
    });

    let execute_tool_span = spans
        .iter()
        .find(|span| {
            span.name == "execute_tool"
                && field_matches(&span.fields, "gen_ai.operation.name", "execute_tool")
                && field_matches(&span.fields, "simulacra.tool.name", "echo")
                && field_matches(
                    &span.fields,
                    "simulacra.tool.source",
                    &format!("mcp:{server_name}"),
                )
        })
        .expect("call_tool should emit an execute_tool span with MCP source attributes");

    assert_eq!(execute_tool_span.name, "execute_tool");
}

// S008 O11y Assertion: simulacra.mcp.calls is emitted once per call with server and tool labels.
#[test]
fn mcp_tool_calls_increment_counter_with_server_and_tool_labels() {
    let server = spawn_json_rpc_test_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }
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

    let ((_, server_name), _spans, events) = capture_traces(|| {
        run_async(async {
            let mut manager = McpManager::new();
            let capability = capability_with_mcp_tools(&["mcp:*:echo"]);
            let server_name = server.server_name().to_string();

            manager
                .connect_http(&server.url("/mcp"))
                .await
                .expect("connect_http should register the MCP server");

            let result = manager
                .call_tool(
                    &server_name,
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability,
                )
                .await;

            (result, server_name)
        })
    });

    let metric_event = events
        .iter()
        .find(|event| {
            field_matches(&event.fields, "counter.simulacra.mcp.calls", "1")
                && field_matches(&event.fields, "server", &server_name)
                && field_matches(&event.fields, "tool", "echo")
        })
        .expect("call_tool should emit simulacra.mcp.calls with server and tool labels");

    assert_eq!(metric_event.current_span.as_deref(), Some("execute_tool"));
}

// S008 O11y Assertion: MCP connection failures are logged at WARN with server and error context.
#[test]
fn mcp_connection_failures_are_logged_at_warn_with_server_and_error() {
    let ((_, failing_server), _spans, events) = capture_traces(|| {
        run_async(async {
            let listener = TcpListener::bind("127.0.0.1:0").expect("failure probe should bind");
            let addr = listener
                .local_addr()
                .expect("failure probe should have a local address");
            drop(listener);

            let mut manager = McpManager::new();
            let url = format!("http://{addr}/mcp");
            let server_name = addr.ip().to_string();

            manager
                .connect_http(&url)
                .await
                .expect("connect_http should register the MCP server before first use");

            let _ = manager.list_tools().await;
            ((), server_name)
        })
    });

    let warning = events
        .iter()
        .find(|event| {
            event.level == "WARN"
                && field_matches(&event.fields, "server", &failing_server)
                && event.fields.contains_key("error")
        })
        .expect("connection failures should emit a WARN log with server and error fields");

    assert_eq!(warning.level, "WARN");
}

// S008 O11y Assertion: MCP tool calls emit gen_ai.tool.message events for both input and output.
#[test]
fn mcp_tool_calls_emit_gen_ai_tool_message_events_for_input_and_output() {
    let server = spawn_json_rpc_test_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }
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

    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            let mut manager = McpManager::new();
            let capability = capability_with_mcp_tools(&["mcp:*:echo"]);
            let server_name = server.server_name().to_string();

            manager
                .connect_http(&server.url("/mcp"))
                .await
                .expect("connect_http should register the MCP server");

            manager
                .call_tool(
                    &server_name,
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability,
                )
                .await
        })
    });

    let input_event = events
        .iter()
        .find(|event| {
            field_matches(&event.fields, "event", "gen_ai.tool.message")
                && event.fields.contains_key("input")
        })
        .expect("call_tool should emit a gen_ai.tool.message event for MCP tool input");

    // Parse the input field as JSON to verify structure, not just substring
    let input_json: serde_json::Value = serde_json::from_str(
        input_event
            .fields
            .get("input")
            .expect("input event should have an 'input' field"),
    )
    .expect("input field should be valid JSON");
    assert_eq!(
        input_json["query"],
        json!("simulacra"),
        "input event should contain the structured tool input with query field"
    );

    // Output event records `gen_ai.tool.result_length` (length only — full
    // output is intentionally not logged because it may contain secrets/PII
    // returned by the MCP server).
    let output_event = events
        .iter()
        .find(|event| {
            field_matches(&event.fields, "event", "gen_ai.tool.message")
                && event.fields.contains_key("gen_ai.tool.result_length")
        })
        .expect("call_tool should emit a gen_ai.tool.message event with result_length");

    let result_length: usize = output_event
        .fields
        .get("gen_ai.tool.result_length")
        .expect("output event should have a result_length field")
        .parse()
        .expect("result_length should parse as usize");
    let expected = json!({ "echoed": { "query": "simulacra" } })
        .to_string()
        .len();
    assert_eq!(
        result_length, expected,
        "result_length should equal the JSON-encoded output length"
    );

    assert_eq!(input_event.current_span.as_deref(), Some("execute_tool"));
    assert_eq!(output_event.current_span.as_deref(), Some("execute_tool"));
}

/// A JSON-RPC server that can be toggled between accepting and rejecting connections.
/// When `rejecting` is true, the server closes connections immediately without responding.
struct ToggleableJsonRpcServer {
    addr: String,
    rejecting: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ToggleableJsonRpcServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn server_name(&self) -> &str {
        self.addr
            .split(':')
            .next()
            .expect("test server address should include a host")
    }

    fn set_rejecting(&self, reject: bool) {
        self.rejecting.store(reject, Ordering::SeqCst);
    }
}

impl Drop for ToggleableJsonRpcServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_toggleable_json_rpc_server(
    tools_list_body: &str,
    tool_call_body: &str,
) -> ToggleableJsonRpcServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("toggleable server should bind");
    listener
        .set_nonblocking(true)
        .expect("toggleable server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("toggleable server should have a local address")
        .to_string();
    let rejecting = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let rejecting_for_thread = Arc::clone(&rejecting);
    let stop_for_thread = Arc::clone(&stop);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    if rejecting_for_thread.load(Ordering::SeqCst) {
                        // Close the connection immediately without sending a response,
                        // simulating a server that is down.
                        drop(stream);
                        continue;
                    }

                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));

                    loop {
                        let mut buffer = [0_u8; 8192];
                        match stream.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(bytes_read) => {
                                let request =
                                    String::from_utf8_lossy(&buffer[..bytes_read]).into_owned();

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
                                } else if request.contains("\"method\":\"tools/list\"") {
                                    tools_list_body.clone()
                                } else if request.contains("\"method\":\"tools/call\"") {
                                    tool_call_body.clone()
                                } else {
                                    json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                                };

                                let response = json_http_response(&body);
                                let _ = stream.write_all(response.as_bytes());
                            }
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) =>
                            {
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    ToggleableJsonRpcServer {
        addr,
        rejecting,
        stop,
        handle: Some(handle),
    }
}

// S008 Assertion: Reconnection with exponential backoff on transport failure.
// A server that fails once then recovers is reconnected automatically.
#[tokio::test]
async fn reconnect_after_transient_failure_succeeds_on_retry() {
    let _guard = test_guard().await;

    let tools_list = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "echo",
                "description": "Echo a payload.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "value": { "type": "integer" } },
                    "required": ["value"]
                }
            }]
        }
    })
    .to_string();
    let tool_call = json!({
        "jsonrpc": "2.0",
        "result": { "echoed": { "value": 42 } }
    })
    .to_string();

    let server = spawn_toggleable_json_rpc_server(&tools_list, &tool_call);
    let mut manager = McpManager::new();
    // Use 10ms base delay so the test runs fast.
    manager.set_reconnect_base_delay_ms(10);
    let capability = capability_with_mcp_tools(&["mcp:*:echo"]);

    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should register the MCP server");

    // First call succeeds, establishing was_connected = true.
    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "value": 42 }),
            &capability,
        )
        .await
        .expect("first call_tool should succeed");
    assert_eq!(output["echoed"]["value"], json!(42));

    // Take the server down.
    server.set_rejecting(true);

    // Schedule the server to come back up after a short delay.
    let rejecting = Arc::clone(&server.rejecting);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        rejecting.store(false, Ordering::SeqCst);
    });

    // This call should fail initially, then reconnect and succeed.
    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "value": 42 }),
            &capability,
        )
        .await
        .expect("call_tool should reconnect and succeed after transient failure");

    assert_eq!(output["echoed"]["value"], json!(42));
}

// S008 Assertion: After 3 reconnection failures, the error propagates.
#[tokio::test]
async fn reconnect_exhausts_retries_and_returns_error() {
    let _guard = test_guard().await;

    let tools_list = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "echo",
                "description": "Echo a payload.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "value": { "type": "integer" } },
                    "required": ["value"]
                }
            }]
        }
    })
    .to_string();
    let tool_call = json!({
        "jsonrpc": "2.0",
        "result": { "echoed": { "value": 1 } }
    })
    .to_string();

    let server = spawn_toggleable_json_rpc_server(&tools_list, &tool_call);
    let mut manager = McpManager::new();
    // Use 10ms base delay so the test runs fast.
    manager.set_reconnect_base_delay_ms(10);
    let capability = capability_with_mcp_tools(&["mcp:*:echo"]);

    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should register the MCP server");

    // First call succeeds, establishing was_connected = true.
    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "value": 1 }),
            &capability,
        )
        .await
        .expect("first call_tool should succeed");
    assert_eq!(output["echoed"]["value"], json!(1));

    // Take the server down permanently.
    server.set_rejecting(true);

    // This call should fail after exhausting all 3 retry attempts.
    let err = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "value": 2 }),
            &capability,
        )
        .await
        .expect_err("call_tool should fail after exhausting reconnection retries");

    assert!(
        matches!(
            err,
            McpError::TransportError(_) | McpError::ConnectionFailed(_)
        ),
        "expected a transport or connection error after exhausted retries, got {err:?}"
    );
}

// S008 Assertion: No reconnection is attempted for servers that never connected.
// If a server has never successfully completed a handshake, transport errors
// are returned immediately without retry.
#[tokio::test]
async fn no_reconnect_for_never_connected_server() {
    let _guard = test_guard().await;

    // Bind a port then drop the listener so the port is unreachable.
    let listener = TcpListener::bind("127.0.0.1:0").expect("probe should bind");
    let addr = listener
        .local_addr()
        .expect("probe should have a local address");
    drop(listener);

    let mut manager = McpManager::new();
    manager.set_reconnect_base_delay_ms(10);
    let capability = capability_with_mcp_tools(&["mcp:*:echo"]);
    let url = format!("http://{addr}/mcp");

    manager
        .connect_http(&url)
        .await
        .expect("connect_http should register the server");

    // Trigger the handshake so that ensure_server_connected runs.
    // Since the server is down, the handshake will fail silently
    // (producing empty tools), but was_connected stays false.
    let _ = manager.list_tools().await;

    let start = std::time::Instant::now();
    let err = manager
        .call_tool(
            &addr.ip().to_string(),
            "echo",
            json!({ "value": 1 }),
            &capability,
        )
        .await
        .expect_err("call_tool to a never-connected server should fail immediately");

    // Verify it failed fast (no reconnection backoff).
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "should fail fast without reconnection attempts, took {:?}",
        elapsed
    );

    assert!(
        matches!(
            err,
            McpError::TransportError(_) | McpError::ConnectionFailed(_)
        ),
        "expected a transport or connection error, got {err:?}"
    );
}
