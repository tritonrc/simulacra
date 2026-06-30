#![allow(
    clippy::type_complexity,
    clippy::await_holding_lock,
    clippy::collapsible_if,
    dead_code
)]

//! Behavioral tests for S024 — MCP Streamable HTTP Transport.
//!
//! Tests the auto-detection handshake: try streamable HTTP POST first,
//! fall back to legacy SSE on 404/405, and explicit transport selection.

use serde_json::json;
use simulacra_mcp::{McpError, McpManager};
use simulacra_types::CapabilityToken;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

// ── Test serialization ──────────────────────────────────────────────
//
// Test servers use non-blocking polling loops that can starve under high
// parallelism. Serialize tests within this binary to avoid flakiness.

static TEST_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    TEST_MUTEX
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

async fn list_tools_with_retry(manager: &mut McpManager) -> Vec<simulacra_mcp::ToolDefinition> {
    for _ in 0..5 {
        let tools = manager.list_tools().await;
        if !tools.is_empty() {
            return tools;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    manager.list_tools().await
}

async fn wait_for_streamable_requests(
    server: &StreamableHttpServer,
    predicate: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    for _ in 0..10 {
        let requests = server.requests();
        if predicate(&requests) {
            return requests;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    server.requests()
}

// ── Helpers ─────────────────────────────────────────────────────────

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

fn json_http_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        body.len(),
        body
    )
}

fn json_http_response_with_session(body: &str, session_id: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: {}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        session_id,
        body.len(),
        body
    )
}

fn http_response_status(status: u16, reason: &str) -> String {
    let body = format!("{{\"error\":\"{reason}\"}}");
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        body.len(),
        body
    )
}

// ── StreamableHttpServer ────────────────────────────────────────────
//
// Fake MCP server supporting the 2025-03-26 streamable HTTP protocol.
// Accepts POST on the configured path. Returns configurable tools and
// optionally sets Mcp-Session-Id.

struct StreamableHttpServer {
    addr: String,
    post_attempts: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StreamableHttpServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn post_attempts(&self) -> usize {
        self.post_attempts.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("request log mutex should not be poisoned")
            .clone()
    }
}

impl Drop for StreamableHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_streamable_http_server(
    tools_list_body: &str,
    tool_call_body: &str,
    session_id: Option<&str>,
) -> StreamableHttpServer {
    let listener =
        TcpListener::bind("127.0.0.1:0").expect("streamable HTTP test server should bind");
    listener
        .set_nonblocking(true)
        .expect("streamable HTTP test server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("streamable HTTP test server should have a local address")
        .to_string();
    let post_attempts = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));

    let post_attempts_for_thread = Arc::clone(&post_attempts);
    let requests_for_thread = Arc::clone(&requests);
    let stop_for_thread = Arc::clone(&stop);
    let ready_for_thread = Arc::clone(&ready);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();
    let session_id = session_id.map(|s| s.to_string());

    let handle = thread::spawn(move || {
        ready_for_thread.store(true, Ordering::SeqCst);
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    while let Some(request) = read_http_request(&mut stream) {
                        if !request.starts_with("POST ") {
                            let response = http_response_status(405, "Method Not Allowed");
                            let _ = stream.write_all(response.as_bytes());
                            break;
                        }

                        post_attempts_for_thread.fetch_add(1, Ordering::SeqCst);
                        requests_for_thread
                            .lock()
                            .expect("request log mutex should not be poisoned")
                            .push(request.clone());

                        let body = if request.contains("\"method\":\"initialize\"") {
                            json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "fake-streamable", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string()
                        } else if request.contains("\"method\":\"notifications/initialized\"") {
                            // Notification — no id, no result needed. Return accepted.
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_body.clone()
                        } else if request.contains("\"method\":\"tools/call\"") {
                            tool_call_body.clone()
                        } else {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        };

                        let response = if request.contains("\"method\":\"initialize\"") {
                            if let Some(ref sid) = session_id {
                                json_http_response_with_session(&body, sid)
                            } else {
                                json_http_response(&body)
                            }
                        } else {
                            json_http_response(&body)
                        };
                        let _ = stream.write_all(response.as_bytes());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    // Wait for the server thread to be ready.
    while !ready.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(1));
    }

    StreamableHttpServer {
        addr,
        post_attempts,
        requests,
        stop,
        handle: Some(handle),
    }
}

// ── FallbackTestServer ──────────────────────────────────────────────
//
// Returns a configurable HTTP status code on POST (simulating a legacy
// SSE-only server that rejects the streamable HTTP initialize POST).
// Serves SSE endpoint discovery on GET /sse, handles JSON-RPC on POST /mcp-rpc.

struct FallbackTestServer {
    addr: String,
    post_attempts: Arc<AtomicUsize>,
    sse_connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FallbackTestServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn post_attempts(&self) -> usize {
        self.post_attempts.load(Ordering::SeqCst)
    }

    fn sse_connections(&self) -> usize {
        self.sse_connections.load(Ordering::SeqCst)
    }
}

impl Drop for FallbackTestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn a server that returns `reject_status` on POST to the SSE URL,
/// but serves proper SSE endpoint discovery on GET and JSON-RPC on
/// POST /mcp-rpc.
fn spawn_fallback_test_server(
    reject_status: u16,
    reject_reason: &str,
    tools_list_body: &str,
) -> FallbackTestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fallback test server should bind");
    listener
        .set_nonblocking(true)
        .expect("fallback test server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("fallback test server should have a local address")
        .to_string();
    let post_attempts = Arc::new(AtomicUsize::new(0));
    let sse_connections = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let ready = Arc::new(AtomicBool::new(false));

    let post_attempts_for_thread = Arc::clone(&post_attempts);
    let sse_connections_for_thread = Arc::clone(&sse_connections);
    let stop_for_thread = Arc::clone(&stop);
    let ready_for_thread = Arc::clone(&ready);
    let reject_reason = reject_reason.to_string();
    let tools_list_body = tools_list_body.to_string();

    let handle = thread::spawn(move || {
        ready_for_thread.store(true, Ordering::SeqCst);
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    let request = match read_http_request(&mut stream) {
                        Some(r) => r,
                        None => continue,
                    };

                    if request.starts_with("GET /sse ") {
                        sse_connections_for_thread.fetch_add(1, Ordering::SeqCst);

                        let stop_for_sse = Arc::clone(&stop_for_thread);
                        let tools_list_body_for_sse = tools_list_body.clone();

                        thread::spawn(move || {
                            // Send SSE endpoint discovery event.
                            let _ = stream.write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\nevent: endpoint\ndata: /mcp-rpc\n\n",
                            );

                            // Now we need to handle POST requests from the client on the
                            // discovered endpoint. But the SSE connection is a different TCP
                            // stream. The client will connect on a new socket for POSTs.
                            // Just keep the SSE stream alive.
                            while !stop_for_sse.load(Ordering::SeqCst) {
                                let _ = stream.write_all(b": keep-alive\n\n");
                                thread::sleep(Duration::from_millis(50));
                            }
                            let _ = tools_list_body_for_sse;
                        });
                        continue;
                    }

                    if request.starts_with("POST /sse ") {
                        // Reject streamable HTTP POST to the SSE URL.
                        post_attempts_for_thread.fetch_add(1, Ordering::SeqCst);
                        let response = http_response_status(reject_status, &reject_reason);
                        let _ = stream.write_all(response.as_bytes());
                        continue;
                    }

                    if request.starts_with("POST /mcp-rpc ") {
                        // Handle JSON-RPC on the discovered endpoint.
                        let body = if request.contains("\"method\":\"initialize\"") {
                            json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2024-11-05",
                                    "serverInfo": { "name": "fake-legacy-sse", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string()
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_body.clone()
                        } else if request.contains("\"method\":\"tools/call\"") {
                            json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string()
                        } else {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        };

                        let response = json_http_response(&body);
                        let _ = stream.write_all(response.as_bytes());
                        continue;
                    }

                    // Unknown request — 404.
                    let response = http_response_status(404, "Not Found");
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    // Wait for the server thread to be ready.
    while !ready.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(1));
    }

    FallbackTestServer {
        addr,
        post_attempts,
        sse_connections,
        stop,
        handle: Some(handle),
    }
}

// ── Tests ───────────────────────────────────────────────────────────

/// S024 Assertion: Auto-detect tries streamable HTTP POST first when `transport` is absent.
///
/// When connecting with `transport = None`, the manager should send a POST
/// with `Accept: application/json, text/event-stream` and protocol version
/// `2025-03-26` before considering any fallback.
#[tokio::test]
async fn autodetect_tries_streamable_http_post_first() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "streamable_tool",
                "description": "A tool served via streamable HTTP",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
        Some("test-session-42"),
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect with None transport should succeed");

    let tools = list_tools_with_retry(&mut manager).await;

    // Verify the server received POST requests (streamable HTTP was tried).
    assert!(
        server.post_attempts() >= 2,
        "auto-detect should POST initialize and tools/list to the server; got {} POST attempts",
        server.post_attempts()
    );

    // Verify tools were discovered.
    assert_eq!(
        tools.len(),
        1,
        "auto-detect via streamable HTTP should discover tools"
    );
    assert_eq!(tools[0].name, "streamable_tool");

    // Verify the Accept header was sent.
    let requests = server.requests();
    let init_request = requests
        .iter()
        .find(|r| r.contains("\"method\":\"initialize\""))
        .expect("server should have received an initialize request");
    assert!(
        init_request.contains("application/json, text/event-stream")
            || init_request.contains("application/json,text/event-stream"),
        "initialize request should include Accept: application/json, text/event-stream; got: {}",
        init_request
    );

    // Verify protocol version is 2025-03-26.
    assert!(
        init_request.contains("2025-03-26"),
        "initialize request should use protocol version 2025-03-26; got: {}",
        init_request
    );
}

#[tokio::test]
async fn transport_auto_is_treated_like_omitted_transport() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "auto_tool",
                "description": "A tool served via explicit auto transport",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
        Some("auto-session-42"),
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), Some("auto"))
        .await
        .expect("transport='auto' should use auto-detect");

    let tools = list_tools_with_retry(&mut manager).await;

    assert!(
        server.post_attempts() >= 2,
        "transport='auto' should POST initialize and tools/list to the server; got {} POST attempts",
        server.post_attempts()
    );
    assert_eq!(tools.len(), 1, "transport='auto' should discover tools");
    assert_eq!(tools[0].name, "auto_tool");
}

#[tokio::test]
async fn transport_auto_falls_back_to_legacy_sse_on_405() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "auto_legacy_tool",
                "description": "A tool served via explicit auto fallback",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(405, "Method Not Allowed", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), Some("auto"))
        .await
        .expect("transport='auto' should register for auto-detect");

    let tools = list_tools_with_retry(&mut manager).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        server.post_attempts() >= 1,
        "transport='auto' should attempt streamable HTTP POST before fallback; got {} POST attempts",
        server.post_attempts()
    );
    assert!(
        server.sse_connections() >= 1,
        "transport='auto' should fall back to SSE after 405; got {} SSE connections",
        server.sse_connections()
    );
    assert_eq!(
        tools.len(),
        1,
        "transport='auto' should discover tools through SSE fallback"
    );
    assert_eq!(tools[0].name, "auto_legacy_tool");
}

/// S024 Assertion: Auto-detect falls back to legacy SSE when streamable HTTP returns 405.
///
/// Server returns 405 on POST to /sse, but serves proper SSE endpoint
/// discovery on GET /sse and JSON-RPC on POST /mcp-rpc. The manager should
/// fall back to legacy SSE and discover tools.
#[tokio::test]
async fn autodetect_falls_back_to_legacy_sse_on_405() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "legacy_tool",
                "description": "A tool served via legacy SSE",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(405, "Method Not Allowed", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), None)
        .await
        .expect("connect with None transport should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    // Give the SSE connection a moment to establish.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Verify the streamable HTTP POST was attempted first.
    assert!(
        server.post_attempts() >= 1,
        "auto-detect should attempt streamable HTTP POST first; got {} POST attempts",
        server.post_attempts()
    );

    // Verify the SSE fallback was used.
    assert!(
        server.sse_connections() >= 1,
        "auto-detect should fall back to SSE after 405; got {} SSE connections",
        server.sse_connections()
    );

    // Verify tools were discovered via SSE fallback.
    assert_eq!(
        tools.len(),
        1,
        "SSE fallback should discover tools from the legacy endpoint"
    );
    assert_eq!(tools[0].name, "legacy_tool");
}

/// S024 Assertion: `transport = "sse"` skips auto-detect and uses legacy SSE directly.
///
/// When the transport is explicitly set to "sse", no streamable HTTP POST
/// should be attempted.
#[tokio::test]
async fn transport_sse_skips_autodetect() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "sse_tool",
                "description": "A tool served via SSE",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    // Use a fallback test server. With transport="sse", the POST should never happen.
    let server = spawn_fallback_test_server(405, "Method Not Allowed", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), Some("sse"))
        .await
        .expect("connect with sse transport should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Verify NO streamable HTTP POST was attempted.
    assert_eq!(
        server.post_attempts(),
        0,
        "transport='sse' should skip auto-detect — no POST attempts expected; got {}",
        server.post_attempts()
    );

    // Verify SSE was used directly.
    assert!(
        server.sse_connections() >= 1,
        "transport='sse' should connect via SSE directly; got {} SSE connections",
        server.sse_connections()
    );

    // Verify tools were discovered.
    assert_eq!(
        tools.len(),
        1,
        "SSE transport should discover tools from the legacy endpoint"
    );
    assert_eq!(tools[0].name, "sse_tool");
}

/// S024 Assertion: `transport = "http"` forces streamable HTTP.
///
/// When the transport is explicitly "http", streamable HTTP is used directly.
#[tokio::test]
async fn transport_http_forces_streamable_http() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "forced_http_tool",
                "description": "A tool forced to use streamable HTTP",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
        None,
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), Some("http"))
        .await
        .expect("connect with http transport should succeed");

    let tools = list_tools_with_retry(&mut manager).await;

    // Verify streamable HTTP was used.
    assert!(
        server.post_attempts() >= 2,
        "transport='http' should POST initialize and tools/list; got {} POST attempts",
        server.post_attempts()
    );

    // Verify tools discovered.
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "forced_http_tool");
}

/// S024 Assertion: `transport = "http"` does not fall back to legacy SSE.
///
/// Even when a server rejects streamable HTTP with a fallback-eligible 405 and
/// would serve legacy SSE, forced HTTP must not switch transports.
#[tokio::test]
async fn transport_http_does_not_fallback_to_legacy_sse_on_405() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "should_not_be_discovered",
                "description": "A tool behind legacy SSE",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(405, "Method Not Allowed", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), Some("http"))
        .await
        .expect("connect with http transport should register without fallback");

    let tools = list_tools_with_retry(&mut manager).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        server.post_attempts() >= 1,
        "transport='http' should attempt streamable HTTP POST; got {} POST attempts",
        server.post_attempts()
    );
    assert_eq!(
        server.sse_connections(),
        0,
        "transport='http' must not fall back to SSE after 405; got {} SSE connections",
        server.sse_connections()
    );
    assert_eq!(
        tools.len(),
        0,
        "transport='http' should not discover tools through SSE fallback"
    );
}

/// S024 Assertion: Both transports failing returns empty tools (graceful degradation).
///
/// When connecting to a dead port with auto-detect, both streamable HTTP
/// and SSE will fail. list_tools should return empty rather than panic.
#[tokio::test]
async fn autodetect_both_fail_returns_empty_tools() {
    let _guard = test_guard().await;
    // Bind a port and immediately drop the listener so nothing is listening.
    let port = {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("ephemeral port should be available");
        listener
            .local_addr()
            .expect("listener should have a local address")
            .port()
    };

    let url = format!("http://127.0.0.1:{}/mcp", port);

    let mut manager = McpManager::new();
    manager
        .connect(&url, None)
        .await
        .expect("connect should succeed (lazy — no network I/O)");

    let tools = list_tools_with_retry(&mut manager).await;

    assert_eq!(
        tools.len(),
        0,
        "when both transports fail, list_tools should return empty tools, not panic"
    );
}

/// S024 Assertion: Mcp-Session-Id from response headers is stored and sent in subsequent requests.
///
/// Verifies that when the server returns Mcp-Session-Id during initialize,
/// subsequent tools/list requests include that session ID.
#[tokio::test]
async fn session_id_stored_from_initialize_response() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "session_tool",
                "description": "A tool that requires a session",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": { "ok": true } }).to_string(),
        Some("session-abc-123"),
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1, "should discover tools via streamable HTTP");

    // Verify that subsequent requests after initialize include the session ID.
    let requests = server.requests();
    let tools_list_request = requests
        .iter()
        .find(|r| r.contains("\"method\":\"tools/list\""))
        .expect("server should have received a tools/list request");

    assert!(
        tools_list_request.contains("Mcp-Session-Id: session-abc-123")
            || tools_list_request.contains("mcp-session-id: session-abc-123"),
        "tools/list request should include Mcp-Session-Id header; got: {}",
        tools_list_request
    );
}

/// S024 Assertion: Auto-detect does NOT fall back on 401 (auth error).
///
/// A 401 response should result in ConnectionFailed, not SSE fallback.
#[tokio::test]
async fn autodetect_does_not_fall_back_on_401() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "auth_tool",
                "description": "Should not be discovered",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    // Server returns 401 on POST (not fallback-eligible).
    let server = spawn_fallback_test_server(401, "Unauthorized", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), None)
        .await
        .expect("connect should succeed (lazy)");

    let tools = list_tools_with_retry(&mut manager).await;

    // The POST was attempted.
    assert!(
        server.post_attempts() >= 1,
        "auto-detect should attempt streamable HTTP POST; got {} POST attempts",
        server.post_attempts()
    );

    // No SSE fallback should occur for auth errors.
    assert_eq!(
        server.sse_connections(),
        0,
        "401 should NOT trigger SSE fallback; got {} SSE connections",
        server.sse_connections()
    );

    // Tools should be empty (handshake failed).
    assert_eq!(
        tools.len(),
        0,
        "auth error should result in no tools, not SSE fallback"
    );
}

/// S024 Assertion: Auto-detect does NOT fall back on 500 (server error).
///
/// A 500 response should result in ConnectionFailed, not SSE fallback.
#[tokio::test]
async fn autodetect_does_not_fall_back_on_500() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "error_tool",
                "description": "Should not be discovered",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(500, "Internal Server Error", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), None)
        .await
        .expect("connect should succeed (lazy)");

    let tools = list_tools_with_retry(&mut manager).await;

    assert!(
        server.post_attempts() >= 1,
        "auto-detect should attempt streamable HTTP POST"
    );

    assert_eq!(
        server.sse_connections(),
        0,
        "500 should NOT trigger SSE fallback; got {} SSE connections",
        server.sse_connections()
    );

    assert_eq!(
        tools.len(),
        0,
        "server error should result in no tools, not SSE fallback"
    );
}

/// S024 Assertion: Protocol version in InitializeRequest is 2025-03-26.
///
/// Verifies the exact protocol version string sent in the initialize request.
#[tokio::test]
async fn initialize_uses_protocol_version_2025_03_26() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": []
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": {} }).to_string(),
        None,
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), Some("http"))
        .await
        .expect("connect should succeed");

    let _ = manager.list_tools().await;

    let requests = wait_for_streamable_requests(&server, |requests| {
        requests
            .iter()
            .any(|r| r.contains("\"method\":\"initialize\""))
    })
    .await;
    let init_request = requests
        .iter()
        .find(|r| r.contains("\"method\":\"initialize\""))
        .expect("server should have received an initialize request");

    assert!(
        init_request.contains("\"protocolVersion\":\"2025-03-26\""),
        "initialize should use protocolVersion 2025-03-26; got: {}",
        init_request
    );
}

/// S024 Assertion: notifications/initialized is sent after successful InitializeResult.
///
/// After the initialize response, the client must send notifications/initialized
/// before tools/list.
#[tokio::test]
async fn initialized_notification_sent_after_initialize() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "ordered_tool",
                "description": "Test ordering",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": {} }).to_string(),
        None,
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1, "handshake should discover tools");

    let requests = wait_for_streamable_requests(&server, |requests| {
        requests
            .iter()
            .any(|r| r.contains("\"method\":\"initialize\""))
            && requests
                .iter()
                .any(|r| r.contains("\"method\":\"notifications/initialized\""))
            && requests
                .iter()
                .any(|r| r.contains("\"method\":\"tools/list\""))
    })
    .await;

    let init_idx = requests
        .iter()
        .position(|r| r.contains("\"method\":\"initialize\""))
        .expect("server should have received an initialize request");

    let notif_idx = requests
        .iter()
        .position(|r| r.contains("\"method\":\"notifications/initialized\""))
        .expect("server should have received a notifications/initialized request");

    let tools_idx = requests
        .iter()
        .position(|r| r.contains("\"method\":\"tools/list\""))
        .expect("server should have received a tools/list request");

    assert!(
        init_idx < notif_idx,
        "initialize (idx {init_idx}) must come before notifications/initialized (idx {notif_idx})"
    );
    assert!(
        notif_idx < tools_idx,
        "notifications/initialized (idx {notif_idx}) must come before tools/list (idx {tools_idx})"
    );
}

/// S024 Assertion: Auto-detect falls back to legacy SSE on 404.
///
/// Same as the 405 test, but verifying 404 also triggers fallback.
#[tokio::test]
async fn autodetect_falls_back_to_legacy_sse_on_404() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "legacy_404_tool",
                "description": "Fallback on 404",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(404, "Not Found", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        server.post_attempts() >= 1,
        "auto-detect should attempt streamable HTTP POST first"
    );
    assert!(
        server.sse_connections() >= 1,
        "auto-detect should fall back to SSE after 404; got {} SSE connections",
        server.sse_connections()
    );
    assert_eq!(
        tools.len(),
        1,
        "SSE fallback should discover tools after 404"
    );
    assert_eq!(tools[0].name, "legacy_404_tool");
}

// ── Helpers for Task 4 tests ───────────────────────────────────────

fn capability_with_mcp_tools(patterns: &[&str]) -> CapabilityToken {
    CapabilityToken {
        mcp_tools: patterns
            .iter()
            .map(|pattern| (*pattern).to_string())
            .collect(),
        ..Default::default()
    }
}

// ── SessionExpiryServer ────────────────────────────────────────────
//
// Fake MCP server that returns 404 on the first tool call (simulating
// session expiry), then succeeds on subsequent handshake + tool calls
// after re-initialization.

struct SessionExpiryServer {
    addr: String,
    tool_call_attempts: Arc<AtomicUsize>,
    initialize_count: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SessionExpiryServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn tool_call_attempts(&self) -> usize {
        self.tool_call_attempts.load(Ordering::SeqCst)
    }

    fn initialize_count(&self) -> usize {
        self.initialize_count.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("mutex").clone()
    }
}

impl Drop for SessionExpiryServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn a server that:
/// - Answers initialize with a session ID
/// - Returns 404 on the first N tool calls (simulating session expiry)
/// - Succeeds on tool calls after that
fn spawn_session_expiry_server(reject_first_n_tool_calls: usize) -> SessionExpiryServer {
    spawn_session_expiry_server_inner(reject_first_n_tool_calls, false)
}

fn spawn_session_expiry_server_with_failed_rehandshake() -> SessionExpiryServer {
    spawn_session_expiry_server_inner(1, true)
}

fn spawn_session_expiry_server_inner(
    reject_first_n_tool_calls: usize,
    fail_rehandshake: bool,
) -> SessionExpiryServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let addr = listener.local_addr().expect("addr").to_string();
    let tool_call_attempts = Arc::new(AtomicUsize::new(0));
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));

    let tool_call_attempts_t = Arc::clone(&tool_call_attempts);
    let initialize_count_t = Arc::clone(&initialize_count);
    let requests_t = Arc::clone(&requests);
    let stop_t = Arc::clone(&stop);
    let ready_t = Arc::clone(&ready);

    let handle = thread::spawn(move || {
        ready_t.store(true, Ordering::SeqCst);
        while !stop_t.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    while let Some(request) = read_http_request(&mut stream) {
                        if !request.starts_with("POST ") {
                            let response = http_response_status(405, "Method Not Allowed");
                            let _ = stream.write_all(response.as_bytes());
                            break;
                        }

                        requests_t.lock().expect("mutex").push(request.clone());

                        if request.contains("\"method\":\"initialize\"") {
                            let initialize_attempt =
                                initialize_count_t.fetch_add(1, Ordering::SeqCst);
                            if fail_rehandshake && initialize_attempt > 0 {
                                let response = http_response_status(503, "Service Unavailable");
                                let _ = stream.write_all(response.as_bytes());
                                break;
                            }
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "session-expiry-test", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let response =
                                json_http_response_with_session(&body, "session-xyz-789");
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"notifications/initialized\"") {
                            let body = json!({ "jsonrpc": "2.0", "result": {} }).to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/list\"") {
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 2,
                                "result": {
                                    "tools": [{
                                        "name": "session_tool",
                                        "description": "A tool for session tests",
                                        "inputSchema": { "type": "object", "properties": {} }
                                    }]
                                }
                            })
                            .to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            let attempt = tool_call_attempts_t.fetch_add(1, Ordering::SeqCst);
                            if attempt < reject_first_n_tool_calls {
                                // Return 404 to simulate session expiry.
                                let response = http_response_status(404, "Not Found");
                                let _ = stream.write_all(response.as_bytes());
                                break; // 404 closes connection
                            } else {
                                let body = json!({
                                    "jsonrpc": "2.0",
                                    "id": 1,
                                    "result": { "session_ok": true }
                                })
                                .to_string();
                                let response = json_http_response(&body);
                                let _ = stream.write_all(response.as_bytes());
                            }
                        } else {
                            let body = json!({ "jsonrpc": "2.0", "result": {} }).to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
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

    while !ready.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(1));
    }

    SessionExpiryServer {
        addr,
        tool_call_attempts,
        initialize_count,
        requests,
        stop,
        handle: Some(handle),
    }
}

// ── SseStreamingServer ─────────────────────────────────────────────
//
// Fake MCP server that returns SSE streaming responses for tool calls.

struct SseStreamingServer {
    addr: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SseStreamingServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for SseStreamingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Type of SSE response for tool calls.
#[derive(Clone)]
enum SseToolResponse {
    /// SSE with progress notifications followed by a final JSON-RPC result.
    WithResult(String),
    /// SSE with only progress notifications, no final result (stream closes).
    NoResult,
}

fn spawn_sse_streaming_server(sse_tool_response: SseToolResponse) -> SseStreamingServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let addr = listener.local_addr().expect("addr").to_string();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));

    let requests_t = Arc::clone(&requests);
    let stop_t = Arc::clone(&stop);
    let ready_t = Arc::clone(&ready);

    let handle = thread::spawn(move || {
        ready_t.store(true, Ordering::SeqCst);
        while !stop_t.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    while let Some(request) = read_http_request(&mut stream) {
                        if !request.starts_with("POST ") {
                            let response = http_response_status(405, "Method Not Allowed");
                            let _ = stream.write_all(response.as_bytes());
                            break;
                        }

                        requests_t.lock().expect("mutex").push(request.clone());

                        if request.contains("\"method\":\"initialize\"") {
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "sse-streaming-test", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let response = json_http_response_with_session(&body, "sse-session-1");
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"notifications/initialized\"") {
                            let body = json!({ "jsonrpc": "2.0", "result": {} }).to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/list\"") {
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 2,
                                "result": {
                                    "tools": [{
                                        "name": "streaming_tool",
                                        "description": "A tool with SSE streaming responses",
                                        "inputSchema": { "type": "object", "properties": {} }
                                    }]
                                }
                            })
                            .to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            // Return an SSE streaming response.
                            let sse_response = match &sse_tool_response {
                                SseToolResponse::WithResult(result_json) => {
                                    // Progress notification + final result.
                                    let progress_event = format!(
                                        "event: message\ndata: {}\n\n",
                                        json!({
                                            "jsonrpc": "2.0",
                                            "method": "notifications/progress",
                                            "params": { "progress": 50, "total": 100 }
                                        })
                                    );
                                    let result_event =
                                        format!("event: message\ndata: {}\n\n", result_json);
                                    format!("{}{}", progress_event, result_event)
                                }
                                SseToolResponse::NoResult => {
                                    // Only progress, then stream closes.
                                    format!(
                                        "event: message\ndata: {}\n\n",
                                        json!({
                                            "jsonrpc": "2.0",
                                            "method": "notifications/progress",
                                            "params": { "progress": 50, "total": 100 }
                                        })
                                    )
                                }
                            };

                            let header = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                                sse_response.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(sse_response.as_bytes());
                            let _ = stream.flush();
                            break; // Close connection after SSE response.
                        } else {
                            let body = json!({ "jsonrpc": "2.0", "result": {} }).to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
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

    while !ready.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(1));
    }

    SseStreamingServer {
        addr,
        requests,
        stop,
        handle: Some(handle),
    }
}

// ── Task 4 Tests ───────────────────────────────────────────────────

/// S024 Assertion: Tool call POST includes Mcp-Session-Id header when session is active.
///
/// Verifies that after a streamable HTTP handshake that returns Mcp-Session-Id,
/// subsequent tool call requests include the session ID header.
#[tokio::test]
async fn session_id_sent_on_tool_calls() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "session_tool",
                "description": "A tool that tracks session ID",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } }).to_string(),
        Some("session-for-tool-call-42"),
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    // Trigger handshake.
    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);

    // Now call the tool.
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = manager
        .call_tool("127.0.0.1", "session_tool", json!({}), &capability)
        .await
        .expect("tool call should succeed");
    assert_eq!(output["ok"], json!(true));

    // Verify the tool call request included the session ID header.
    let requests = server.requests();
    let tool_call_request = requests
        .iter()
        .find(|r| r.contains("\"method\":\"tools/call\""))
        .expect("server should have received a tools/call request");

    assert!(
        tool_call_request.contains("Mcp-Session-Id: session-for-tool-call-42")
            || tool_call_request.contains("mcp-session-id: session-for-tool-call-42"),
        "tools/call request should include Mcp-Session-Id header; got: {}",
        tool_call_request
    );
}

/// S024 Assertion: HTTP 404 with stored session ID triggers session expiry handling.
///
/// Server returns 404 on the first tool call, which should trigger re-handshake
/// (no backoff) and a retry that succeeds.
#[tokio::test]
async fn session_expiry_on_404_triggers_rehandshake() {
    let _guard = test_guard().await;

    let server = spawn_session_expiry_server(1); // reject first tool call with 404

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    // Trigger initial handshake.
    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "session_tool");

    // First initialize happened during list_tools.
    assert_eq!(
        server.initialize_count(),
        1,
        "one initialize should have happened during list_tools"
    );

    // Call the tool — first attempt gets 404 (session expired),
    // should re-handshake and retry.
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = manager
        .call_tool("127.0.0.1", "session_tool", json!({}), &capability)
        .await
        .expect("tool call should succeed after session expiry re-handshake");

    assert_eq!(output["session_ok"], json!(true));

    // Verify re-handshake happened: there should be 2 initialize requests total.
    assert!(
        server.initialize_count() >= 2,
        "session expiry should trigger a re-handshake (second initialize); got {} initializations",
        server.initialize_count()
    );

    // Verify the tool call was attempted at least twice.
    assert!(
        server.tool_call_attempts() >= 2,
        "should have attempted tool call at least twice (first rejected, second succeeded); got {}",
        server.tool_call_attempts()
    );
}

/// S024 Assertion: Failed re-handshake after session expiry returns an error.
///
/// A failed session-expiry re-handshake must not fall through to a raw
/// tools/call dispatch with no established transport mode.
#[tokio::test]
async fn session_expiry_failed_rehandshake_returns_error_without_retry_dispatch() {
    let _guard = test_guard().await;

    let server = spawn_session_expiry_server_with_failed_rehandshake();

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "session_tool");

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool("127.0.0.1", "session_tool", json!({}), &capability)
        .await
        .expect_err("failed session re-handshake should return an error");

    assert!(
        matches!(err, McpError::ConnectionFailed(ref msg) if msg.contains("127.0.0.1")),
        "expected ConnectionFailed mentioning the server after failed re-handshake, got {err:?}"
    );
    assert!(
        server.initialize_count() >= 2,
        "session expiry should attempt a re-handshake; got {} initializations",
        server.initialize_count()
    );
    assert_eq!(
        server.tool_call_attempts(),
        1,
        "failed re-handshake must not dispatch a retry tools/call without a transport"
    );
}

/// S024 Assertion: text/event-stream response is parsed as SSE, progress logged, final result extracted.
///
/// Server returns a tool call response with Content-Type: text/event-stream containing
/// progress notifications and a final JSON-RPC result.
#[tokio::test]
async fn sse_streaming_response_extracts_final_result() {
    let _guard = test_guard().await;

    let result_json = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "streamed_value": "hello from SSE" }
    })
    .to_string();

    let server = spawn_sse_streaming_server(SseToolResponse::WithResult(result_json));

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "streaming_tool");

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = manager
        .call_tool("127.0.0.1", "streaming_tool", json!({}), &capability)
        .await
        .expect("SSE streaming tool call should succeed");

    assert_eq!(
        output["streamed_value"],
        json!("hello from SSE"),
        "should extract the final result from the SSE stream"
    );
}

/// S024 Assertion: SSE stream closing without a JSON-RPC response returns McpError::ProtocolError.
///
/// Server sends only progress notifications via SSE, then closes the stream
/// without a final JSON-RPC result.
#[tokio::test]
async fn sse_stream_no_result_returns_protocol_error() {
    let _guard = test_guard().await;

    let server = spawn_sse_streaming_server(SseToolResponse::NoResult);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool("127.0.0.1", "streaming_tool", json!({}), &capability)
        .await
        .expect_err("SSE stream without result should fail");

    assert!(
        matches!(&err, McpError::ProtocolError(msg) if msg.contains("SSE stream closed")),
        "should get ProtocolError about SSE stream closing without result; got: {err:?}"
    );
}

// ── ReconnectionServer ────────────────────────────────────────────
//
// Fake MCP server that returns a transient transport failure on the first
// tool call, then succeeds on subsequent handshakes + tool calls after
// reconnection.

struct ReconnectionServer {
    addr: String,
    tool_call_attempts: Arc<AtomicUsize>,
    initialize_count: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ReconnectionServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn initialize_count(&self) -> usize {
        self.initialize_count.load(Ordering::SeqCst)
    }

    fn tool_call_attempts(&self) -> usize {
        self.tool_call_attempts.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("mutex").clone()
    }
}

impl Drop for ReconnectionServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn a server that:
/// - Answers initialize/tools_list normally (streamable HTTP)
/// - Returns HTTP 503 on the first tool call (transport error)
/// - Succeeds on all subsequent requests (after client reconnects)
fn spawn_reconnection_server() -> ReconnectionServer {
    spawn_reconnection_server_inner(false)
}

fn spawn_reconnection_server_with_failed_rehandshake() -> ReconnectionServer {
    spawn_reconnection_server_inner(true)
}

fn spawn_reconnection_server_inner(fail_rehandshake: bool) -> ReconnectionServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let addr = listener.local_addr().expect("addr").to_string();
    let tool_call_attempts = Arc::new(AtomicUsize::new(0));
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));

    let tool_call_attempts_t = Arc::clone(&tool_call_attempts);
    let initialize_count_t = Arc::clone(&initialize_count);
    let requests_t = Arc::clone(&requests);
    let stop_t = Arc::clone(&stop);
    let ready_t = Arc::clone(&ready);

    let handle = thread::spawn(move || {
        ready_t.store(true, Ordering::SeqCst);
        while !stop_t.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    while let Some(request) = read_http_request(&mut stream) {
                        if !request.starts_with("POST ") {
                            let response = http_response_status(405, "Method Not Allowed");
                            let _ = stream.write_all(response.as_bytes());
                            break;
                        }

                        requests_t.lock().expect("mutex").push(request.clone());

                        if request.contains("\"method\":\"initialize\"") {
                            let initialize_attempt =
                                initialize_count_t.fetch_add(1, Ordering::SeqCst);
                            if fail_rehandshake && initialize_attempt > 0 {
                                let response = http_response_status(503, "Service Unavailable");
                                let _ = stream.write_all(response.as_bytes());
                                break;
                            }
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "reconnect-test", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"notifications/initialized\"") {
                            let body = json!({ "jsonrpc": "2.0", "result": {} }).to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/list\"") {
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 2,
                                "result": {
                                    "tools": [{
                                        "name": "reconnect_tool",
                                        "description": "A tool for reconnection tests",
                                        "inputSchema": { "type": "object", "properties": {} }
                                    }]
                                }
                            })
                            .to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            let attempt = tool_call_attempts_t.fetch_add(1, Ordering::SeqCst);
                            if attempt == 0 {
                                let response = http_response_status(503, "Service Unavailable");
                                let _ = stream.write_all(response.as_bytes());
                                let _ = stream.shutdown(std::net::Shutdown::Both);
                                break;
                            }
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": { "reconnected": true }
                            })
                            .to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
                        } else {
                            let body = json!({ "jsonrpc": "2.0", "result": {} }).to_string();
                            let response = json_http_response(&body);
                            let _ = stream.write_all(response.as_bytes());
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

    while !ready.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(1));
    }

    ReconnectionServer {
        addr,
        tool_call_attempts,
        initialize_count,
        requests,
        stop,
        handle: Some(handle),
    }
}

// ── Task 5-7 Tests ────────────────────────────────────────────────

/// S024 Assertion: Reconnection after transport failure re-handshakes via streamable HTTP.
///
/// Server returns a transport failure on the first tool call. After reconnection,
/// the client should re-handshake using streamable HTTP (not SSE fallback)
/// and successfully complete the tool call.
#[tokio::test]
async fn reconnection_preserves_transport_mode() {
    let _guard = test_guard().await;

    let server = spawn_reconnection_server();

    let mut manager = McpManager::new();
    manager.set_reconnect_base_delay_ms(10);
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    // Trigger initial handshake via list_tools.
    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "reconnect_tool");

    // First initialize happened during list_tools.
    assert_eq!(
        server.initialize_count(),
        1,
        "one initialize should have happened during list_tools"
    );

    // Call the tool. First attempt drops connection; reconnection should succeed.
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = manager
        .call_tool("127.0.0.1", "reconnect_tool", json!({}), &capability)
        .await
        .expect("tool call should succeed after reconnection");

    assert_eq!(output["reconnected"], json!(true));

    // Verify reconnection happened: multiple initialize requests.
    assert!(
        server.initialize_count() >= 2,
        "reconnection should trigger a re-handshake (second initialize); got {} initializations",
        server.initialize_count()
    );

    // Verify all initialize requests used streamable HTTP (POST method,
    // not SSE). Every request in our log is a POST (SSE would be GET).
    let requests = server.requests();
    let init_requests: Vec<&String> = requests
        .iter()
        .filter(|r| r.contains("\"method\":\"initialize\""))
        .collect();

    assert!(
        init_requests.len() >= 2,
        "server should have received at least 2 initialize requests; got {}",
        init_requests.len()
    );

    // All initialize requests should be POST (streamable HTTP), not GET (SSE).
    for (i, req) in init_requests.iter().enumerate() {
        assert!(
            req.starts_with("POST "),
            "initialize request #{} should be a POST (streamable HTTP); got: {}",
            i + 1,
            &req[..req.len().min(80)]
        );
    }
}

/// S024 Assertion: Failed reconnection handshake returns an error.
///
/// A failed reconnection handshake must not fall through to a raw tools/call
/// dispatch with `transport_mode = None`.
#[tokio::test]
async fn reconnection_failed_rehandshake_returns_error_without_retry_dispatch() {
    let _guard = test_guard().await;

    let server = spawn_reconnection_server_with_failed_rehandshake();

    let mut manager = McpManager::new();
    manager.set_reconnect_base_delay_ms(10);
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "reconnect_tool");

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let err = manager
        .call_tool("127.0.0.1", "reconnect_tool", json!({}), &capability)
        .await
        .expect_err("failed reconnection handshake should return an error");

    assert!(
        matches!(err, McpError::ConnectionFailed(ref msg) if msg.contains("127.0.0.1")),
        "expected ConnectionFailed mentioning the server after failed reconnection, got {err:?}"
    );
    assert!(
        server.initialize_count() >= 2,
        "reconnection should attempt a re-handshake; got {} initializations",
        server.initialize_count()
    );
    assert_eq!(
        server.tool_call_attempts(),
        1,
        "failed reconnection handshake must not dispatch a retry tools/call without a transport"
    );
}

/// S024 Assertion: Session expiry uses immediate re-handshake, not backoff.
///
/// When a tool call returns 404 (session expired), the client should
/// re-handshake immediately without waiting for the exponential backoff
/// delay. Set a huge backoff and verify the operation completes quickly.
#[tokio::test]
async fn session_expiry_has_no_backoff_delay() {
    let _guard = test_guard().await;

    let server = spawn_session_expiry_server(1); // reject first tool call with 404

    let mut manager = McpManager::new();
    // Set a huge backoff — if session expiry used this, the test would time out.
    manager.set_reconnect_base_delay_ms(30_000);
    manager
        .connect(&server.url("/mcp"), None)
        .await
        .expect("connect should succeed");

    // Trigger initial handshake.
    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "session_tool");

    let capability = capability_with_mcp_tools(&["mcp:*:*"]);

    // Time the tool call. Session expiry should re-handshake immediately.
    let start = std::time::Instant::now();
    let output = manager
        .call_tool("127.0.0.1", "session_tool", json!({}), &capability)
        .await
        .expect("tool call should succeed after session expiry re-handshake");

    let elapsed = start.elapsed();

    assert_eq!(output["session_ok"], json!(true));

    // The entire operation should complete well under 5 seconds.
    // If the 30s backoff were used, this would take >= 30s.
    assert!(
        elapsed < Duration::from_secs(5),
        "session expiry should use immediate re-handshake, not 30s backoff; took {:?}",
        elapsed
    );

    // Verify re-handshake actually happened.
    assert!(
        server.initialize_count() >= 2,
        "session expiry should trigger re-handshake; got {} initializations",
        server.initialize_count()
    );
}

/// S024 Assertion: Existing SSE configuration works unchanged (backwards compatibility).
///
/// Using `connect(&url, Some("sse"))` with a server that serves legacy SSE
/// should discover tools without any streamable HTTP attempts.
#[tokio::test]
async fn existing_sse_config_works_unchanged() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "legacy_sse_tool",
                "description": "A tool served via legacy SSE transport",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(405, "Method Not Allowed", &tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/sse"), Some("sse"))
        .await
        .expect("connect with sse transport should succeed");

    let tools = list_tools_with_retry(&mut manager).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Verify tools were discovered.
    assert!(
        !tools.is_empty(),
        "SSE transport should discover tools from the legacy endpoint"
    );
    assert_eq!(tools[0].name, "legacy_sse_tool");

    // Verify NO streamable HTTP POST was attempted (SSE was used directly).
    assert_eq!(
        server.post_attempts(),
        0,
        "transport='sse' should skip auto-detect entirely — no POST attempts expected; got {}",
        server.post_attempts()
    );

    // Verify SSE was used.
    assert!(
        server.sse_connections() >= 1,
        "transport='sse' should connect via SSE; got {} SSE connections",
        server.sse_connections()
    );
}

/// S024 Assertion: Tool call with application/json response is correctly parsed.
///
/// Basic streamable HTTP tool call where the server returns a JSON response
/// (not SSE streaming). Verifies the result is correctly extracted.
#[tokio::test]
async fn tool_call_json_response_works() {
    let _guard = test_guard().await;
    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [{
                "name": "json_tool",
                "description": "A tool that returns JSON directly",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "input_value": { "type": "string" }
                    }
                }
            }]
        }
    })
    .to_string();

    let tool_call_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "computed": "result-from-json-path",
            "code": 200
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(&tools_body, &tool_call_body, None);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url("/mcp"), Some("http"))
        .await
        .expect("connect with http transport should succeed");

    // Trigger handshake.
    let tools = list_tools_with_retry(&mut manager).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "json_tool");

    // Call the tool.
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);
    let output = manager
        .call_tool(
            "127.0.0.1",
            "json_tool",
            json!({ "input_value": "test" }),
            &capability,
        )
        .await
        .expect("tool call with JSON response should succeed");

    // Verify the result was correctly parsed from the JSON response.
    assert_eq!(
        output["computed"],
        json!("result-from-json-path"),
        "JSON response result should be correctly extracted"
    );
    assert_eq!(
        output["code"],
        json!(200),
        "JSON response should preserve all result fields"
    );
}
