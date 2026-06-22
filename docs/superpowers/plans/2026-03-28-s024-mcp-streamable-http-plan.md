# S024 MCP Streamable HTTP Transport — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace MCP transport layer with 2025-03-26 streamable HTTP protocol, with auto-detect fallback to legacy SSE.

**Architecture:** Introduce `TransportMode` enum on `McpConnection` to replace flat SSE/HTTP fields. Auto-detection tries streamable HTTP POST first, falls back to legacy SSE on 404/405. Public API (`list_tools`, `call_tool`) unchanged. SSE response stream parsing handles both `application/json` and `text/event-stream` content types from tool calls.

**Tech Stack:** Rust, reqwest (HTTP client + streaming), tokio (async runtime), serde_json, tracing, opentelemetry

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/simulacra-mcp/src/lib.rs` | Modify | `TransportMode` enum, `McpConnection` refactor, auto-detect handshake, streamable HTTP dispatch, SSE response parsing, session management, new OTel meters |
| `crates/simulacra-config/src/lib.rs` | Modify | Make `McpServerConfig.transport` optional |
| `crates/simulacra-mcp/tests/s024_streamable_http_red.rs` | Create | All S024 behavioral tests |
| `crates/simulacra-mcp/Cargo.toml` | Modify | No new deps needed (reqwest already has `stream` feature) |

---

### Task 1: Make `McpServerConfig.transport` optional

**Files:**
- Modify: `crates/simulacra-config/src/lib.rs:106-115`

This is a config-only change. Making `transport` optional enables auto-detection when omitted.

- [ ] **Step 1: Write the failing test**

Add to `crates/simulacra-config/src/lib.rs` in the existing `#[cfg(test)] mod tests` block:

```rust
#[test]
fn mcp_server_config_transport_is_optional() {
    let toml_str = r#"
        [[mcp.servers]]
        name = "autodetect-server"
        url = "https://example.com/mcp"
    "#;

    let config: SimulacraConfig = toml::from_str(&format!(
        r#"
        [project]
        name = "test"

        [agent_types.default]
        model = "test-model"
        system_prompt = "test"

        {toml_str}
        "#
    ))
    .expect("config with no transport field should parse");

    let mcp = config.mcp.expect("mcp section should exist");
    assert_eq!(mcp.servers.len(), 1);
    assert_eq!(mcp.servers[0].name, "autodetect-server");
    assert!(
        mcp.servers[0].transport.is_none(),
        "transport should be None when omitted"
    );
}

#[test]
fn mcp_server_config_transport_explicit_sse_still_works() {
    let config: SimulacraConfig = toml::from_str(
        r#"
        [project]
        name = "test"

        [agent_types.default]
        model = "test-model"
        system_prompt = "test"

        [[mcp.servers]]
        name = "legacy"
        transport = "sse"
        url = "https://example.com/sse"
        "#,
    )
    .expect("config with explicit transport should parse");

    let mcp = config.mcp.expect("mcp section should exist");
    assert_eq!(mcp.servers[0].transport.as_deref(), Some("sse"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p simulacra-config mcp_server_config_transport_is_optional`
Expected: FAIL — `transport` is currently `String` (required), not `Option<String>`

- [ ] **Step 3: Change `transport` to `Option<String>`**

In `crates/simulacra-config/src/lib.rs`, change the `McpServerConfig` struct:

```rust
/// A single MCP server entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
}
```

- [ ] **Step 4: Fix any compilation errors from the type change**

Search for all uses of `.transport` on `McpServerConfig` across the workspace and update them to handle `Option<String>`. This may include CLI wiring code that matches on the transport string.

Run: `cargo build --workspace`
Expected: PASS (or find and fix callers)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p simulacra-config`
Expected: PASS — both new tests and existing tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-config/src/lib.rs
git commit -m "feat(config): make McpServerConfig.transport optional [S024]"
```

---

### Task 2: Introduce `TransportMode` enum and refactor `McpConnection`

**Files:**
- Modify: `crates/simulacra-mcp/src/lib.rs:83-101` (McpConnection struct)

Replace the flat `sse_handle` and `post_endpoint` fields with a `TransportMode` enum. This is a refactor — no behavioral changes yet, existing tests must still pass.

- [ ] **Step 1: Add the `TransportMode` enum and update `McpConnection`**

In `crates/simulacra-mcp/src/lib.rs`, add the enum before `McpConnection`:

```rust
/// Transport mode for an MCP connection, determined during handshake.
#[derive(Debug)]
enum TransportMode {
    /// 2025-03-26 streamable HTTP — single endpoint, optional session ID.
    StreamableHttp {
        session_id: Option<String>,
    },
    /// 2024-11-05 legacy SSE — discovered POST endpoint, persistent stream.
    LegacySse {
        post_endpoint: String,
        sse_handle: tokio::task::JoinHandle<()>,
    },
    /// 2024-11-05 legacy HTTP — plain request/response (no SSE).
    LegacyHttp,
}
```

Update `McpConnection`:

```rust
/// State for a single connected MCP server.
#[derive(Debug)]
struct McpConnection {
    #[allow(dead_code)]
    server_name: String,
    url: String,
    tools: Vec<McpToolSchema>,
    handshake_done: bool,
    was_connected: bool,
    /// Transport mode, determined during handshake. None before first handshake.
    transport_mode: Option<TransportMode>,
    /// Configured transport preference from simulacra.toml.
    /// None = auto-detect, Some("sse") = legacy SSE, Some("http") = streamable HTTP.
    configured_transport: Option<String>,
}
```

- [ ] **Step 2: Update `connect_sse` to use `configured_transport`**

```rust
pub async fn connect_sse(&mut self, url: &str) -> Result<(), McpError> {
    let parsed = url::Url::parse(url).map_err(|e| {
        let server = url;
        let error = e.to_string();
        tracing::warn!(server = server, error = %error, "WARN: MCP connection failure");
        McpError::ConnectionFailed(format!("sse connection to {url} failed: {e}"))
    })?;

    let server_name = parsed.host_str().unwrap_or("unknown").to_string();

    self.connections.insert(
        server_name.clone(),
        McpConnection {
            server_name,
            url: url.to_string(),
            tools: Vec::new(),
            handshake_done: false,
            was_connected: false,
            transport_mode: None,
            configured_transport: Some("sse".to_string()),
        },
    );

    Ok(())
}
```

- [ ] **Step 3: Update `connect_http` to use `configured_transport`**

```rust
pub async fn connect_http(&mut self, url: &str) -> Result<(), McpError> {
    let parsed = url::Url::parse(url).map_err(|e| {
        let server = url;
        let error = e.to_string();
        tracing::warn!(server = server, error = %error, "WARN: MCP connection failure");
        McpError::ConnectionFailed(format!("http connection to {url} failed: {e}"))
    })?;

    let server_name = parsed.host_str().unwrap_or("unknown").to_string();

    self.connections.insert(
        server_name.clone(),
        McpConnection {
            server_name,
            url: url.to_string(),
            tools: Vec::new(),
            handshake_done: false,
            was_connected: false,
            transport_mode: None,
            configured_transport: Some("http".to_string()),
        },
    );

    Ok(())
}
```

- [ ] **Step 4: Add `connect` method for auto-detect (no configured transport)**

```rust
/// Register an MCP server URL for auto-detect transport negotiation.
///
/// On first use, the manager will try streamable HTTP first, falling
/// back to legacy SSE if the server returns 404 or 405.
pub async fn connect(&mut self, url: &str, transport: Option<&str>) -> Result<(), McpError> {
    let parsed = url::Url::parse(url).map_err(|e| {
        let error = e.to_string();
        tracing::warn!(server = url, error = %error, "WARN: MCP connection failure");
        McpError::ConnectionFailed(format!("connection to {url} failed: {e}"))
    })?;

    let server_name = parsed.host_str().unwrap_or("unknown").to_string();

    self.connections.insert(
        server_name.clone(),
        McpConnection {
            server_name,
            url: url.to_string(),
            tools: Vec::new(),
            handshake_done: false,
            was_connected: false,
            transport_mode: None,
            configured_transport: transport.map(|s| s.to_string()),
        },
    );

    Ok(())
}
```

- [ ] **Step 5: Update `ensure_connected` and `ensure_server_connected` to use `configured_transport`**

Replace the URL-sniffing `is_sse` logic with explicit dispatch based on `configured_transport`:

```rust
async fn ensure_connected(&mut self) {
    let keys: Vec<String> = self
        .connections
        .keys()
        .filter(|k| {
            self.connections
                .get(*k)
                .map(|c| !c.handshake_done)
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    for key in keys {
        self.handshake_server(&key).await;
    }
}

async fn ensure_server_connected(&mut self, server: &str) {
    let needs_handshake = self
        .connections
        .get(server)
        .map(|c| !c.handshake_done)
        .unwrap_or(false);

    if !needs_handshake {
        return;
    }

    self.handshake_server(server).await;
}
```

- [ ] **Step 6: Add `handshake_server` that dispatches based on `configured_transport`**

For now, this preserves the existing behavior exactly — SSE config goes to SSE handshake, everything else goes to HTTP handshake. Auto-detect will be added in Task 3.

```rust
/// Dispatch handshake based on configured transport preference.
async fn handshake_server(&mut self, key: &str) {
    let conn = match self.connections.get(key) {
        Some(c) => c,
        None => return,
    };
    let configured = conn.configured_transport.clone();

    match configured.as_deref() {
        Some("sse") => {
            self.perform_sse_handshake(key).await;
        }
        _ => {
            // HTTP or auto-detect — for now, use legacy HTTP handshake.
            // Auto-detect (streamable HTTP with fallback) added in Task 3.
            let url = match self.connections.get(key) {
                Some(c) => c.url.clone(),
                None => return,
            };
            let tools = match self.perform_http_handshake(&url).await {
                Ok(t) => t,
                Err(e) => {
                    let server_name = self
                        .connections
                        .get(key)
                        .map(|c| c.server_name.clone())
                        .unwrap_or_else(|| key.clone());
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "MCP connection failure"
                    );
                    Vec::new()
                }
            };
            if let Some(conn) = self.connections.get_mut(key) {
                conn.tools = tools;
                conn.handshake_done = true;
                conn.was_connected = true;
                conn.transport_mode = Some(TransportMode::LegacyHttp);
            }
        }
    }
}
```

- [ ] **Step 7: Update `perform_sse_handshake` to set `TransportMode::LegacySse`**

At the end of `perform_sse_handshake`, replace the direct field writes with:

```rust
if let Some(conn) = self.connections.get_mut(key) {
    conn.transport_mode = Some(TransportMode::LegacySse {
        post_endpoint: post_endpoint.unwrap_or_default(),
        sse_handle,
    });
    conn.tools = tools;
    conn.handshake_done = true;
    conn.was_connected = true;
}
```

- [ ] **Step 8: Update `dispatch_tool_call` to use `TransportMode`**

```rust
async fn dispatch_tool_call(
    &self,
    server: &str,
    tool: &str,
    input: &serde_json::Value,
) -> Result<serde_json::Value, McpError> {
    let conn = self.connections.get(server).ok_or_else(|| {
        McpError::ConnectionFailed(format!("no connection to server {server}"))
    })?;

    // Determine target URL based on transport mode.
    let target_url = match &conn.transport_mode {
        Some(TransportMode::LegacySse { post_endpoint, .. }) => post_endpoint.clone(),
        Some(TransportMode::StreamableHttp { .. }) | Some(TransportMode::LegacyHttp) | None => {
            conn.url.clone()
        }
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| McpError::TransportError(e.to_string()))?;

    let call_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": input,
        }
    });

    let response = client
        .post(&target_url)
        .json(&call_request)
        .send()
        .await
        .map_err(|e| McpError::TransportError(e.to_string()))?;

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| McpError::ProtocolError(e.to_string()))?;

    Ok(body.get("result").cloned().unwrap_or_else(|| body.clone()))
}
```

- [ ] **Step 9: Update `dispatch_with_reconnect` to reset `transport_mode` instead of old fields**

In the reconnection loop, replace:

```rust
conn.handshake_done = false;
conn.sse_handle = None;
conn.post_endpoint = None;
```

With:

```rust
conn.handshake_done = false;
conn.transport_mode = None;
```

- [ ] **Step 10: Run all existing tests**

Run: `cargo test -p simulacra-mcp`
Expected: PASS — all 17 existing S008 tests still pass. This is a pure refactor.

Run: `cargo build --workspace`
Expected: PASS

- [ ] **Step 11: Commit**

```bash
git add crates/simulacra-mcp/src/lib.rs
git commit -m "refactor(mcp): introduce TransportMode enum, add connect() method [S024]"
```

---

### Task 3: Streamable HTTP handshake with auto-detect fallback

**Files:**
- Modify: `crates/simulacra-mcp/src/lib.rs`
- Create: `crates/simulacra-mcp/tests/s024_streamable_http_red.rs`

Implement the core auto-detection: try streamable HTTP POST, fall back to legacy SSE on 404/405.

- [ ] **Step 1: Write the failing tests**

Create `crates/simulacra-mcp/tests/s024_streamable_http_red.rs`:

```rust
//! S024 — MCP Streamable HTTP Transport behavioral tests.
//!
//! Tests cover: auto-detection, streamable HTTP handshake, session management,
//! SSE response parsing, reconnection, and observability.

use simulacra_mcp::{McpError, McpManager};
use simulacra_types::{AgentId, CapabilityToken};
use serde_json::json;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

fn capability_with_mcp_tools(patterns: &[&str]) -> CapabilityToken {
    CapabilityToken {
        mcp_tools: patterns.iter().map(|p| (*p).to_string()).collect(),
        ..Default::default()
    }
}

fn run_async<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime should build")
        .block_on(future)
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
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

/// A streamable HTTP MCP server that responds to POST requests on a single endpoint.
/// Returns 2025-03-26 protocol version and optional Mcp-Session-Id.
struct StreamableHttpServer {
    addr: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StreamableHttpServer {
    fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    fn server_name(&self) -> &str {
        self.addr
            .split(':')
            .next()
            .expect("server address should include a host")
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
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
    let listener = TcpListener::bind("127.0.0.1:0").expect("server should bind");
    listener
        .set_nonblocking(true)
        .expect("server should become nonblocking");

    let addr = listener.local_addr().unwrap().to_string();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let requests_thread = Arc::clone(&requests);
    let stop_thread = Arc::clone(&stop);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();
    let session_id = session_id.map(|s| s.to_string());

    let handle = thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    loop {
                        let request = match read_http_request(&mut stream) {
                            Some(r) => r,
                            None => break,
                        };

                        requests_thread.lock().unwrap().push(request.clone());

                        let body = if request.contains("\"method\":\"initialize\"") {
                            json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "streamable-mcp", "version": "1.0.0" },
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

                        let response = if let Some(ref sid) = session_id {
                            json_http_response_with_session(&body, sid)
                        } else {
                            json_http_response(&body)
                        };
                        if stream.write_all(response.as_bytes()).is_err() {
                            break;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    StreamableHttpServer {
        addr,
        requests,
        stop,
        handle: Some(handle),
    }
}

/// A server that returns 405 on POST (simulating a legacy SSE-only server),
/// then serves SSE endpoint discovery and JSON-RPC on discovered POST endpoint.
struct FallbackTestServer {
    addr: String,
    post_attempts: Arc<AtomicUsize>,
    sse_connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FallbackTestServer {
    fn url(&self) -> String {
        format!("http://{}/sse", self.addr)
    }

    fn server_name(&self) -> &str {
        self.addr
            .split(':')
            .next()
            .expect("server address should include a host")
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

fn spawn_fallback_test_server(tools_list_body: &str) -> FallbackTestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("server should bind");
    listener
        .set_nonblocking(true)
        .expect("server should become nonblocking");

    let addr = listener.local_addr().unwrap().to_string();
    let post_attempts = Arc::new(AtomicUsize::new(0));
    let sse_connections = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let post_attempts_thread = Arc::clone(&post_attempts);
    let sse_connections_thread = Arc::clone(&sse_connections);
    let stop_thread = Arc::clone(&stop);
    let tools_list_body = tools_list_body.to_string();

    let handle = thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    let request = match read_http_request(&mut stream) {
                        Some(r) => r,
                        None => continue,
                    };

                    if request.starts_with("POST /sse ") {
                        // Reject POST — this is a legacy SSE-only server
                        post_attempts_thread.fetch_add(1, Ordering::SeqCst);
                        let response = http_response_status(405, "Method Not Allowed");
                        let _ = stream.write_all(response.as_bytes());
                        continue;
                    }

                    if request.starts_with("GET /sse ") {
                        sse_connections_thread.fetch_add(1, Ordering::SeqCst);

                        // Send SSE endpoint discovery
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\nevent: endpoint\ndata: /mcp-rpc\n\n",
                        );

                        let stop_sse = Arc::clone(&stop_thread);
                        thread::spawn(move || {
                            while !stop_sse.load(Ordering::SeqCst) {
                                if stream.write_all(b": keep-alive\n\n").is_err() {
                                    break;
                                }
                                thread::sleep(Duration::from_millis(50));
                            }
                        });
                        continue;
                    }

                    if request.starts_with("POST /mcp-rpc ") {
                        let body = if request.contains("\"method\":\"initialize\"") {
                            json!({
                                "jsonrpc": "2.0",
                                "result": {
                                    "protocolVersion": "2024-11-05",
                                    "serverInfo": { "name": "legacy-sse-mcp", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string()
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_body.clone()
                        } else {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        };

                        let response = json_http_response(&body);
                        let _ = stream.write_all(response.as_bytes());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    FallbackTestServer {
        addr,
        post_attempts,
        sse_connections,
        stop,
        handle: Some(handle),
    }
}

// ── S024 Assertion: Auto-detect tries streamable HTTP POST first ──

#[tokio::test]
async fn autodetect_tries_streamable_http_post_first() {
    let tools_body = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "stream_tool",
                "description": "A streamable HTTP tool",
                "inputSchema": { "type": "object" }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": {} }).to_string(),
        Some("test-session-001"),
    );

    let mut manager = McpManager::new();
    manager
        .connect(&server.url(), None)
        .await
        .expect("connect should succeed");

    let tools = manager.list_tools().await;
    assert_eq!(tools.len(), 1, "should discover tools via streamable HTTP");
    assert_eq!(tools[0].name, "stream_tool");

    // Verify the initialize request was sent as POST with correct Accept header
    let requests = server.requests();
    let init_request = requests
        .iter()
        .find(|r| r.contains("\"method\":\"initialize\""))
        .expect("should have sent initialize request");
    assert!(
        init_request.contains("Accept: application/json, text/event-stream")
            || init_request.contains("accept: application/json, text/event-stream"),
        "initialize POST should include Accept header for content negotiation"
    );
}

// ── S024 Assertion: Auto-detect falls back to legacy SSE on 404/405 ──

#[tokio::test]
async fn autodetect_falls_back_to_legacy_sse_on_405() {
    let tools_body = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "legacy_tool",
                "description": "A legacy SSE tool",
                "inputSchema": { "type": "object" }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(&tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url(), None)
        .await
        .expect("connect should succeed");

    let tools = manager.list_tools().await;
    assert_eq!(tools.len(), 1, "should discover tools via legacy SSE fallback");
    assert_eq!(tools[0].name, "legacy_tool");

    // Verify auto-detect tried POST first, then fell back to SSE
    assert!(
        server.post_attempts() >= 1,
        "should have tried POST first (streamable HTTP attempt)"
    );
    assert!(
        server.sse_connections() >= 1,
        "should have fallen back to SSE after POST was rejected"
    );
}

// ── S024 Assertion: transport="sse" skips auto-detect ──

#[tokio::test]
async fn transport_sse_skips_autodetect() {
    let tools_body = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "sse_tool",
                "description": "Direct SSE tool",
                "inputSchema": { "type": "object" }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(&tools_body);

    let mut manager = McpManager::new();
    // Force SSE transport — should NOT try POST first
    manager
        .connect(&server.url(), Some("sse"))
        .await
        .expect("connect should succeed");

    let tools = manager.list_tools().await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "sse_tool");

    // Should NOT have tried POST (auto-detect skipped)
    assert_eq!(
        server.post_attempts(),
        0,
        "transport=sse should skip auto-detect POST attempt"
    );
    assert!(
        server.sse_connections() >= 1,
        "should have gone directly to SSE"
    );
}

// ── S024 Assertion: transport="http" uses streamable HTTP, no fallback ──

#[tokio::test]
async fn transport_http_forces_streamable_no_fallback() {
    let tools_body = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "http_tool",
                "description": "Streamable HTTP tool",
                "inputSchema": { "type": "object" }
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
        .connect(&server.url(), Some("http"))
        .await
        .expect("connect should succeed");

    let tools = manager.list_tools().await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "http_tool");
}

// ── S024 Assertion: Both transports failing returns ConnectionFailed ──

#[tokio::test]
async fn autodetect_both_fail_returns_connection_failed() {
    // Connect to a port that's not listening
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener); // close it immediately — nothing listening

    let mut manager = McpManager::new();
    manager
        .connect(&format!("http://{addr}/mcp"), None)
        .await
        .expect("connect is lazy — should succeed");

    let tools = manager.list_tools().await;
    // Both attempts fail — should get no tools (connection failure is non-fatal in list_tools)
    assert!(tools.is_empty(), "should have no tools when both transports fail");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p simulacra-mcp s024`
Expected: FAIL — `connect` method exists but auto-detect and streamable HTTP handshake not implemented yet

- [ ] **Step 3: Implement `perform_streamable_http_handshake`**

Add to `McpManager` in `crates/simulacra-mcp/src/lib.rs`:

```rust
/// Perform the 2025-03-26 streamable HTTP handshake.
///
/// POSTs initialize to the server URL with content negotiation headers.
/// If the server returns Mcp-Session-Id, it is stored for subsequent requests.
///
/// Returns `Ok(tools)` on success, or `Err` if the handshake fails.
/// A 404 or 405 from the server is returned as `McpError::TransportError`
/// so the caller can fall back to legacy SSE.
async fn perform_streamable_http_handshake(
    &self,
    url: &str,
) -> Result<(Vec<McpToolSchema>, Option<String>), McpError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| McpError::TransportError(e.to_string()))?;

    // Step 1: Initialize
    let initialize_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "simulacra",
                "version": "0.1.0"
            }
        }
    });

    let init_response = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .json(&initialize_request)
        .send()
        .await
        .map_err(|e| McpError::TransportError(e.to_string()))?;

    let status = init_response.status().as_u16();

    // 404 or 405 = server doesn't support streamable HTTP
    if status == 404 || status == 405 {
        return Err(McpError::TransportError(format!(
            "server returned {status} — does not support streamable HTTP"
        )));
    }

    // Other 4xx = auth/permission issue, not transport mismatch
    if (400..500).contains(&status) {
        return Err(McpError::ConnectionFailed(format!(
            "server returned {status} during streamable HTTP handshake"
        )));
    }

    // 5xx = server error
    if status >= 500 {
        return Err(McpError::ConnectionFailed(format!(
            "server returned {status} during streamable HTTP handshake"
        )));
    }

    // Extract session ID from response headers
    let session_id = init_response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Step 2: Send notifications/initialized
    let initialized_notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });

    let mut notif_req = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .json(&initialized_notification);
    if let Some(ref sid) = session_id {
        notif_req = notif_req.header("Mcp-Session-Id", sid);
    }
    let _ = notif_req
        .send()
        .await
        .map_err(|e| McpError::TransportError(e.to_string()))?;

    // Step 3: tools/list
    let tools_list_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });

    let mut tools_req = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .json(&tools_list_request);
    if let Some(ref sid) = session_id {
        tools_req = tools_req.header("Mcp-Session-Id", sid);
    }

    let tools_response = tools_req
        .send()
        .await
        .map_err(|e| McpError::TransportError(e.to_string()))?;

    let body: serde_json::Value = tools_response
        .json()
        .await
        .map_err(|e| McpError::ProtocolError(e.to_string()))?;

    let tools = body
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<McpToolSchema>(v.clone()).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok((tools, session_id))
}
```

- [ ] **Step 4: Implement auto-detect in `handshake_server`**

Update `handshake_server` to try streamable HTTP first, fall back to legacy SSE:

```rust
async fn handshake_server(&mut self, key: &str) {
    let conn = match self.connections.get(key) {
        Some(c) => c,
        None => return,
    };
    let configured = conn.configured_transport.clone();
    let url = conn.url.clone();

    match configured.as_deref() {
        Some("sse") => {
            // Forced legacy SSE — skip auto-detect
            self.perform_sse_handshake(key).await;
        }
        Some("http") => {
            // Forced streamable HTTP — no fallback
            let _span = tracing::info_span!(
                "simulacra_mcp_handshake",
                simulacra.mcp.transport_mode = "streamable_http",
                simulacra.mcp.protocol_version = "2025-03-26",
                simulacra.mcp.session_id = tracing::field::Empty,
            )
            .entered();

            match self.perform_streamable_http_handshake(&url).await {
                Ok((tools, session_id)) => {
                    if let Some(ref sid) = session_id {
                        tracing::Span::current().record("simulacra.mcp.session_id", sid.as_str());
                    }
                    if let Some(conn) = self.connections.get_mut(key) {
                        conn.tools = tools;
                        conn.handshake_done = true;
                        conn.was_connected = true;
                        conn.transport_mode =
                            Some(TransportMode::StreamableHttp { session_id });
                    }
                }
                Err(e) => {
                    let server_name = self
                        .connections
                        .get(key)
                        .map(|c| c.server_name.clone())
                        .unwrap_or_else(|| key.clone());
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "MCP streamable HTTP handshake failure"
                    );
                }
            }
        }
        _ => {
            // Auto-detect: try streamable HTTP first, fall back to legacy SSE
            let _span = tracing::info_span!(
                "simulacra_mcp_handshake",
                simulacra.mcp.transport_mode = tracing::field::Empty,
                simulacra.mcp.protocol_version = tracing::field::Empty,
                simulacra.mcp.session_id = tracing::field::Empty,
            )
            .entered();

            match self.perform_streamable_http_handshake(&url).await {
                Ok((tools, session_id)) => {
                    tracing::Span::current()
                        .record("simulacra.mcp.transport_mode", "streamable_http");
                    tracing::Span::current()
                        .record("simulacra.mcp.protocol_version", "2025-03-26");
                    if let Some(ref sid) = session_id {
                        tracing::Span::current().record("simulacra.mcp.session_id", sid.as_str());
                    }
                    if let Some(conn) = self.connections.get_mut(key) {
                        conn.tools = tools;
                        conn.handshake_done = true;
                        conn.was_connected = true;
                        conn.transport_mode =
                            Some(TransportMode::StreamableHttp { session_id });
                    }
                    return;
                }
                Err(McpError::TransportError(msg))
                    if msg.contains("404") || msg.contains("405") =>
                {
                    let server_name = self
                        .connections
                        .get(key)
                        .map(|c| c.server_name.clone())
                        .unwrap_or_else(|| key.clone());
                    tracing::info!(
                        server = %server_name,
                        "Streamable HTTP not supported, falling back to legacy SSE"
                    );
                }
                Err(e) => {
                    // Non-fallback error (auth, server error) — don't try SSE
                    let server_name = self
                        .connections
                        .get(key)
                        .map(|c| c.server_name.clone())
                        .unwrap_or_else(|| key.clone());
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "MCP connection failure"
                    );
                    return;
                }
            }

            // Fallback: try legacy SSE
            tracing::Span::current().record("simulacra.mcp.transport_mode", "legacy_sse");
            tracing::Span::current().record("simulacra.mcp.protocol_version", "2024-11-05");
            self.perform_sse_handshake(key).await;
        }
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p simulacra-mcp s024`
Expected: PASS — all auto-detection tests pass

Run: `cargo test -p simulacra-mcp`
Expected: PASS — existing S008 tests still pass

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-mcp/src/lib.rs crates/simulacra-mcp/tests/s024_streamable_http_red.rs
git commit -m "feat(mcp): streamable HTTP handshake with auto-detect fallback [S024]"
```

---

### Task 4: Session management (`Mcp-Session-Id`)

**Files:**
- Modify: `crates/simulacra-mcp/src/lib.rs`
- Modify: `crates/simulacra-mcp/tests/s024_streamable_http_red.rs`

Implement session ID propagation on tool calls, and session expiry handling on 404.

- [ ] **Step 1: Write the failing tests**

Add to `crates/simulacra-mcp/tests/s024_streamable_http_red.rs`:

```rust
// ── S024 Assertion: Session ID stored and sent on subsequent requests ──

#[tokio::test]
async fn session_id_stored_and_sent_on_subsequent_requests() {
    let tools_body = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "echo",
                "description": "Echo",
                "inputSchema": { "type": "object" }
            }]
        }
    })
    .to_string();

    let server = spawn_streamable_http_server(
        &tools_body,
        &json!({ "jsonrpc": "2.0", "result": { "echoed": true } }).to_string(),
        Some("session-abc-123"),
    );

    let mut manager = McpManager::new();
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);

    manager
        .connect(&server.url(), Some("http"))
        .await
        .expect("connect should succeed");

    // Trigger handshake + tool call
    let result = manager
        .call_tool(server.server_name(), "echo", json!({}), &capability)
        .await
        .expect("call_tool should succeed");
    assert_eq!(result["echoed"], json!(true));

    // Verify session ID was sent on the tool call request
    let requests = server.requests();
    let tool_call_request = requests
        .iter()
        .find(|r| r.contains("\"method\":\"tools/call\""))
        .expect("should have sent a tools/call request");
    assert!(
        tool_call_request.contains("Mcp-Session-Id: session-abc-123")
            || tool_call_request.contains("mcp-session-id: session-abc-123"),
        "tool call should include Mcp-Session-Id header"
    );
}

// ── S024 Assertion: 404 triggers session expiry and re-handshake ──

#[tokio::test]
async fn session_expiry_on_404_triggers_rehandshake() {
    let tools_body = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "echo",
                "description": "Echo",
                "inputSchema": { "type": "object" }
            }]
        }
    })
    .to_string();

    // Server that returns 404 on the first tool call, then works on retry
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let call_count = Arc::new(AtomicUsize::new(0));

    let stop_thread = Arc::clone(&stop);
    let call_count_thread = Arc::clone(&call_count);
    let tools_body_clone = tools_body.clone();

    let handle = thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    loop {
                        let request = match read_http_request(&mut stream) {
                            Some(r) => r,
                            None => break,
                        };

                        if request.contains("\"method\":\"initialize\"") {
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "expiry-mcp", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let response = json_http_response_with_session(&body, "new-session");
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/list\"") {
                            let response =
                                json_http_response_with_session(&tools_body_clone, "new-session");
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            let count = call_count_thread.fetch_add(1, Ordering::SeqCst);
                            if count == 0 {
                                // First tool call: return 404 (session expired)
                                let response = http_response_status(404, "Not Found");
                                let _ = stream.write_all(response.as_bytes());
                            } else {
                                // Subsequent calls: succeed
                                let body = json!({
                                    "jsonrpc": "2.0",
                                    "result": { "retry_worked": true }
                                })
                                .to_string();
                                let response =
                                    json_http_response_with_session(&body, "new-session");
                                let _ = stream.write_all(response.as_bytes());
                            }
                        } else {
                            let response = json_http_response(
                                &json!({ "jsonrpc": "2.0", "result": {} }).to_string(),
                            );
                            let _ = stream.write_all(response.as_bytes());
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let mut manager = McpManager::new();
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);

    manager
        .connect(&format!("http://{addr}/mcp"), Some("http"))
        .await
        .unwrap();

    let result = manager
        .call_tool("127.0.0.1", "echo", json!({}), &capability)
        .await
        .expect("should recover from session expiry via re-handshake");

    assert_eq!(
        result["retry_worked"],
        json!(true),
        "tool call should succeed after session expiry recovery"
    );

    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p simulacra-mcp session_id_stored`
Run: `cargo test -p simulacra-mcp session_expiry`
Expected: FAIL — session ID not sent on tool calls, 404 handling not implemented

- [ ] **Step 3: Update `dispatch_tool_call` to send session ID and handle SSE responses**

```rust
async fn dispatch_tool_call(
    &self,
    server: &str,
    tool: &str,
    input: &serde_json::Value,
) -> Result<serde_json::Value, McpError> {
    let conn = self.connections.get(server).ok_or_else(|| {
        McpError::ConnectionFailed(format!("no connection to server {server}"))
    })?;

    // Determine target URL and session ID based on transport mode.
    let (target_url, session_id) = match &conn.transport_mode {
        Some(TransportMode::LegacySse { post_endpoint, .. }) => {
            (post_endpoint.clone(), None)
        }
        Some(TransportMode::StreamableHttp { session_id }) => {
            (conn.url.clone(), session_id.clone())
        }
        Some(TransportMode::LegacyHttp) | None => {
            (conn.url.clone(), None)
        }
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| McpError::TransportError(e.to_string()))?;

    let call_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": input,
        }
    });

    let mut req = client
        .post(&target_url)
        .header("Accept", "application/json, text/event-stream")
        .json(&call_request);

    if let Some(ref sid) = session_id {
        req = req.header("Mcp-Session-Id", sid);
    }

    let response = req
        .send()
        .await
        .map_err(|e| McpError::TransportError(e.to_string()))?;

    let status = response.status().as_u16();

    // Session expiry: 404 with a session ID present
    if status == 404 && session_id.is_some() {
        return Err(McpError::ProtocolError(
            "session expired (404)".to_string(),
        ));
    }

    if status >= 400 {
        return Err(McpError::TransportError(format!(
            "server returned HTTP {status}"
        )));
    }

    // Check Content-Type for SSE vs JSON response
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    if content_type.contains("text/event-stream") {
        // SSE streaming response — buffer events, extract final JSON-RPC result
        self.parse_sse_tool_response(response, server).await
    } else {
        // Standard JSON response
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| McpError::ProtocolError(e.to_string()))?;

        Ok(body.get("result").cloned().unwrap_or_else(|| body.clone()))
    }
}
```

- [ ] **Step 4: Add `parse_sse_tool_response` for streaming responses**

```rust
/// Parse an SSE streaming response from a tool call.
///
/// Buffers SSE events, logs progress notifications via tracing::debug!,
/// and extracts the final JSON-RPC response.
async fn parse_sse_tool_response(
    &self,
    mut response: reqwest::Response,
    server: &str,
) -> Result<serde_json::Value, McpError> {
    let mut accumulated = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(McpError::TransportError(
                "SSE stream idle timeout (60s)".to_string(),
            ));
        }

        match tokio::time::timeout(remaining, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                accumulated.push_str(&String::from_utf8_lossy(&chunk));

                // Try to parse complete SSE events from accumulated data
                while let Some((event_type, event_data, rest)) =
                    parse_next_sse_event(&accumulated)
                {
                    accumulated = rest;

                    if event_type.as_deref() == Some("message") || event_type.is_none() {
                        // Try to parse as JSON-RPC
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&event_data)
                        {
                            if json.get("result").is_some() || json.get("error").is_some() {
                                // Final JSON-RPC response
                                return Ok(json
                                    .get("result")
                                    .cloned()
                                    .unwrap_or_else(|| json.clone()));
                            } else if json.get("method").is_some() {
                                // Progress notification
                                tracing::debug!(
                                    server = %server,
                                    method = json["method"].as_str().unwrap_or("unknown"),
                                    "MCP SSE progress notification"
                                );
                            }
                        }
                    }
                }
            }
            Ok(Ok(None)) => {
                // Stream ended
                return Err(McpError::ProtocolError(
                    "SSE stream closed without delivering a JSON-RPC response".to_string(),
                ));
            }
            Ok(Err(e)) => {
                return Err(McpError::TransportError(e.to_string()));
            }
            Err(_) => {
                return Err(McpError::TransportError(
                    "SSE stream idle timeout (60s)".to_string(),
                ));
            }
        }
    }
}
```

- [ ] **Step 5: Add `parse_next_sse_event` helper**

```rust
/// Parse the next complete SSE event from accumulated text.
///
/// Returns (event_type, data, remaining_text) if a complete event is found.
/// A complete event is delimited by a blank line.
fn parse_next_sse_event(text: &str) -> Option<(Option<String>, String, String)> {
    // Find the end of the first event (blank line = "\n\n")
    let event_end = text.find("\n\n")?;
    let event_text = &text[..event_end];
    let rest = text[event_end + 2..].to_string();

    let mut event_type = None;
    let mut data_lines = Vec::new();

    for line in event_text.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event_type = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim().to_string());
        }
    }

    if data_lines.is_empty() {
        return None;
    }

    Some((event_type, data_lines.join("\n"), rest))
}
```

- [ ] **Step 6: Implement session expiry handling in `dispatch_with_reconnect`**

Add session expiry detection before the reconnection loop. When a `ProtocolError` containing "session expired" is returned, clear the session and re-handshake immediately without backoff:

```rust
async fn dispatch_with_reconnect(
    &mut self,
    server: &str,
    tool: &str,
    input: &serde_json::Value,
) -> Result<serde_json::Value, McpError> {
    const MAX_RETRIES: u32 = 3;
    let base_backoff_ms = self.reconnect_base_delay_ms;

    let first_err = match self.dispatch_tool_call(server, tool, input).await {
        Ok(output) => return Ok(output),
        Err(e) => e,
    };

    // Session expiry: immediate re-handshake, no backoff
    if matches!(&first_err, McpError::ProtocolError(msg) if msg.contains("session expired")) {
        tracing::info!(
            server = %server,
            "MCP session expired, re-initializing"
        );

        // S024: increment session_expired counter
        let meters = McpMeters::get();
        meters.session_expired.add(
            1,
            &[KeyValue::new("server", server.to_owned())],
        );

        if let Some(conn) = self.connections.get_mut(server) {
            conn.handshake_done = false;
            conn.transport_mode = None;
        }

        self.ensure_server_connected(server).await;

        // Retry the original request once
        return self.dispatch_tool_call(server, tool, input).await;
    }

    // Not a transport error — don't attempt reconnection
    if !self.is_transport_error(&first_err) {
        return Err(first_err);
    }

    // Only attempt reconnection if the server had previously connected.
    let was_connected = self
        .connections
        .get(server)
        .map(|c| c.was_connected)
        .unwrap_or(false);

    if !was_connected {
        return Err(first_err);
    }

    tracing::warn!(
        server = %server,
        error = %first_err,
        "MCP connection lost, attempting reconnection"
    );

    let mut last_err = first_err;

    for attempt in 0..MAX_RETRIES {
        let backoff = std::time::Duration::from_millis(base_backoff_ms * (1 << attempt));
        tokio::time::sleep(backoff).await;

        if let Some(conn) = self.connections.get_mut(server) {
            conn.handshake_done = false;
            conn.transport_mode = None;
        }

        self.ensure_server_connected(server).await;

        match self.dispatch_tool_call(server, tool, input).await {
            Ok(output) => {
                tracing::info!(
                    server = %server,
                    attempt = attempt + 1,
                    "MCP reconnection succeeded"
                );
                return Ok(output);
            }
            Err(e) => {
                tracing::warn!(
                    server = %server,
                    attempt = attempt + 1,
                    error = %e,
                    "MCP reconnection attempt failed"
                );
                last_err = e;
            }
        }
    }

    Err(last_err)
}
```

- [ ] **Step 7: Add `session_expired` counter to `McpMeters`**

```rust
struct McpMeters {
    tool_duration: Histogram<f64>,
    calls: Counter<u64>,
    tool_errors: Counter<u64>,
    session_expired: Counter<u64>,
}

impl McpMeters {
    fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<McpMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-mcp");
            McpMeters {
                tool_duration: meter
                    .f64_histogram("simulacra.mcp.tool.duration")
                    .with_unit("ms")
                    .with_description("MCP tool call duration")
                    .build(),
                calls: meter
                    .u64_counter("simulacra.mcp.calls")
                    .with_description("Total MCP tool calls")
                    .build(),
                tool_errors: meter
                    .u64_counter("simulacra.mcp.tool.errors")
                    .with_description("Total MCP tool call errors")
                    .build(),
                session_expired: meter
                    .u64_counter("simulacra.mcp.session_expired")
                    .with_description("MCP session expiry events")
                    .build(),
            }
        })
    }
}
```

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p simulacra-mcp s024`
Expected: PASS — session ID propagation and expiry recovery tests pass

Run: `cargo test -p simulacra-mcp`
Expected: PASS — all tests pass

- [ ] **Step 9: Commit**

```bash
git add crates/simulacra-mcp/src/lib.rs crates/simulacra-mcp/tests/s024_streamable_http_red.rs
git commit -m "feat(mcp): session management and SSE response parsing [S024]"
```

---

### Task 5: SSE streaming tool call response tests

**Files:**
- Modify: `crates/simulacra-mcp/tests/s024_streamable_http_red.rs`
- Modify: `crates/simulacra-mcp/src/lib.rs` (if fixes needed)

Verify that tool calls returning `text/event-stream` responses are correctly parsed.

- [ ] **Step 1: Write the failing tests**

Add a `StreamingSseServer` to the test file and corresponding tests:

```rust
/// An MCP server that returns tool call responses as SSE streams
/// with progress notifications before the final result.
struct StreamingSseServer {
    addr: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StreamingSseServer {
    fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    fn server_name(&self) -> &str {
        self.addr.split(':').next().unwrap()
    }
}

impl Drop for StreamingSseServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_streaming_sse_server(tools_list_body: &str) -> StreamingSseServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let stop = Arc::new(AtomicBool::new(false));

    let stop_thread = Arc::clone(&stop);
    let tools_list_body = tools_list_body.to_string();

    let handle = thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    loop {
                        let request = match read_http_request(&mut stream) {
                            Some(r) => r,
                            None => break,
                        };

                        if request.contains("\"method\":\"initialize\"") {
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "streaming-mcp", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let _ = stream.write_all(json_http_response(&body).as_bytes());
                        } else if request.contains("\"method\":\"tools/list\"") {
                            let _ =
                                stream.write_all(json_http_response(&tools_list_body).as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            // Return SSE stream with progress + final result
                            let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                            let _ = stream.write_all(header.as_bytes());

                            // Progress notification
                            let progress = format!(
                                "data: {}\n\n",
                                json!({
                                    "jsonrpc": "2.0",
                                    "method": "notifications/progress",
                                    "params": { "progress": 50, "total": 100 }
                                })
                            );
                            let _ = stream.write_all(progress.as_bytes());

                            thread::sleep(Duration::from_millis(50));

                            // Final result
                            let result = format!(
                                "data: {}\n\n",
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": 1,
                                    "result": { "streaming_worked": true }
                                })
                            );
                            let _ = stream.write_all(result.as_bytes());
                            break; // Close stream after result
                        } else {
                            let _ = stream.write_all(
                                json_http_response(
                                    &json!({ "jsonrpc": "2.0", "result": {} }).to_string(),
                                )
                                .as_bytes(),
                            );
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    StreamingSseServer {
        addr,
        stop,
        handle: Some(handle),
    }
}

// ── S024 Assertion: SSE stream parsed, progress logged, final result extracted ──

#[tokio::test]
async fn sse_streaming_response_extracts_final_result() {
    let tools_body = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "slow_tool",
                "description": "A tool with streaming progress",
                "inputSchema": { "type": "object" }
            }]
        }
    })
    .to_string();

    let server = spawn_streaming_sse_server(&tools_body);

    let mut manager = McpManager::new();
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);

    manager
        .connect(&server.url(), Some("http"))
        .await
        .unwrap();

    let result = manager
        .call_tool(server.server_name(), "slow_tool", json!({}), &capability)
        .await
        .expect("streaming SSE response should be parsed correctly");

    assert_eq!(
        result["streaming_worked"],
        json!(true),
        "should extract the final JSON-RPC result from SSE stream"
    );
}

// ── S024 Assertion: SSE stream closing without result returns ProtocolError ──

#[tokio::test]
async fn sse_stream_no_result_returns_protocol_error() {
    // Server that sends SSE stream with only progress, then closes
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let stop = Arc::new(AtomicBool::new(false));

    let stop_thread = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    loop {
                        let request = match read_http_request(&mut stream) {
                            Some(r) => r,
                            None => break,
                        };

                        if request.contains("\"method\":\"initialize\"") {
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "broken-mcp", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let _ = stream.write_all(json_http_response(&body).as_bytes());
                        } else if request.contains("\"method\":\"tools/list\"") {
                            let body = json!({
                                "jsonrpc": "2.0",
                                "result": {
                                    "tools": [{
                                        "name": "broken",
                                        "description": "Broken",
                                        "inputSchema": { "type": "object" }
                                    }]
                                }
                            })
                            .to_string();
                            let _ = stream.write_all(json_http_response(&body).as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            // Send SSE header + progress only, then close
                            let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
                            let _ = stream.write_all(header.as_bytes());
                            let progress = format!(
                                "data: {}\n\n",
                                json!({
                                    "jsonrpc": "2.0",
                                    "method": "notifications/progress"
                                })
                            );
                            let _ = stream.write_all(progress.as_bytes());
                            break; // Close without sending result
                        } else {
                            let _ = stream.write_all(
                                json_http_response(
                                    &json!({ "jsonrpc": "2.0", "result": {} }).to_string(),
                                )
                                .as_bytes(),
                            );
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let mut manager = McpManager::new();
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);

    manager
        .connect(&format!("http://{addr}/mcp"), Some("http"))
        .await
        .unwrap();

    let err = manager
        .call_tool("127.0.0.1", "broken", json!({}), &capability)
        .await
        .expect_err("should fail when SSE stream closes without result");

    assert!(
        matches!(err, McpError::ProtocolError(msg) if msg.contains("closed without")),
        "should return ProtocolError, got: {err:?}"
    );

    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p simulacra-mcp s024`
Expected: PASS — these should work with the implementation from Task 4

- [ ] **Step 3: Commit**

```bash
git add crates/simulacra-mcp/tests/s024_streamable_http_red.rs
git commit -m "test(mcp): SSE streaming response parsing tests [S024]"
```

---

### Task 6: Reconnection is transport-mode-aware

**Files:**
- Modify: `crates/simulacra-mcp/tests/s024_streamable_http_red.rs`
- Modify: `crates/simulacra-mcp/src/lib.rs` (if fixes needed)

Verify that reconnection preserves transport mode and re-handshakes correctly.

- [ ] **Step 1: Write the failing tests**

Add to `crates/simulacra-mcp/tests/s024_streamable_http_red.rs`:

```rust
// ── S024 Assertion: Reconnection stays in current transport mode ──

#[tokio::test]
async fn reconnection_preserves_transport_mode() {
    let tools_body = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "reconnect_tool",
                "description": "Test reconnection",
                "inputSchema": { "type": "object" }
            }]
        }
    })
    .to_string();

    // Server that fails on first tool call, succeeds on second (after reconnection)
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let call_count = Arc::new(AtomicUsize::new(0));
    let handshake_count = Arc::new(AtomicUsize::new(0));

    let stop_thread = Arc::clone(&stop);
    let call_count_thread = Arc::clone(&call_count);
    let handshake_count_thread = Arc::clone(&handshake_count);
    let tools_body_clone = tools_body.clone();

    let handle = thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    loop {
                        let request = match read_http_request(&mut stream) {
                            Some(r) => r,
                            None => break,
                        };

                        if request.contains("\"method\":\"initialize\"") {
                            handshake_count_thread.fetch_add(1, Ordering::SeqCst);
                            // Verify it's a streamable HTTP handshake (2025-03-26)
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "reconnect-mcp", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let _ = stream.write_all(json_http_response(&body).as_bytes());
                        } else if request.contains("\"method\":\"tools/list\"") {
                            let _ = stream
                                .write_all(json_http_response(&tools_body_clone).as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            let count = call_count_thread.fetch_add(1, Ordering::SeqCst);
                            if count == 0 {
                                // First call: simulate transport error (close connection)
                                break;
                            } else {
                                let body = json!({
                                    "jsonrpc": "2.0",
                                    "result": { "reconnected": true }
                                })
                                .to_string();
                                let _ =
                                    stream.write_all(json_http_response(&body).as_bytes());
                            }
                        } else {
                            let _ = stream.write_all(
                                json_http_response(
                                    &json!({ "jsonrpc": "2.0", "result": {} }).to_string(),
                                )
                                .as_bytes(),
                            );
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let mut manager = McpManager::new();
    manager.set_reconnect_base_delay_ms(10); // Fast retries for tests
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);

    manager
        .connect(&format!("http://{addr}/mcp"), Some("http"))
        .await
        .unwrap();

    let result = manager
        .call_tool("127.0.0.1", "reconnect_tool", json!({}), &capability)
        .await
        .expect("should succeed after reconnection");

    assert_eq!(result["reconnected"], json!(true));

    // Verify reconnection re-handshaked with streamable HTTP (multiple initialize calls)
    assert!(
        handshake_count.load(Ordering::SeqCst) >= 2,
        "should have re-handshaked after transport failure"
    );

    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}

// ── S024 Assertion: Session expiry triggers immediate re-handshake (no backoff) ──

#[tokio::test]
async fn session_expiry_has_no_backoff_delay() {
    let tools_body = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "fast_tool",
                "description": "Test",
                "inputSchema": { "type": "object" }
            }]
        }
    })
    .to_string();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let call_count = Arc::new(AtomicUsize::new(0));

    let stop_thread = Arc::clone(&stop);
    let call_count_thread = Arc::clone(&call_count);
    let tools_body_clone = tools_body.clone();

    let handle = thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    loop {
                        let request = match read_http_request(&mut stream) {
                            Some(r) => r,
                            None => break,
                        };

                        if request.contains("\"method\":\"initialize\"") {
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "serverInfo": { "name": "fast-mcp", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string();
                            let response =
                                json_http_response_with_session(&body, "session-fast");
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/list\"") {
                            let response = json_http_response_with_session(
                                &tools_body_clone,
                                "session-fast",
                            );
                            let _ = stream.write_all(response.as_bytes());
                        } else if request.contains("\"method\":\"tools/call\"") {
                            let count = call_count_thread.fetch_add(1, Ordering::SeqCst);
                            if count == 0 {
                                let response = http_response_status(404, "Not Found");
                                let _ = stream.write_all(response.as_bytes());
                            } else {
                                let body = json!({
                                    "jsonrpc": "2.0",
                                    "result": { "fast_recovery": true }
                                })
                                .to_string();
                                let response =
                                    json_http_response_with_session(&body, "session-fast-v2");
                                let _ = stream.write_all(response.as_bytes());
                            }
                        } else {
                            let _ = stream.write_all(
                                json_http_response(
                                    &json!({ "jsonrpc": "2.0", "result": {} }).to_string(),
                                )
                                .as_bytes(),
                            );
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let mut manager = McpManager::new();
    // Set a HUGE reconnect delay — if session expiry uses backoff, this test will timeout
    manager.set_reconnect_base_delay_ms(30_000);
    let capability = capability_with_mcp_tools(&["mcp:*:*"]);

    manager
        .connect(&format!("http://{addr}/mcp"), Some("http"))
        .await
        .unwrap();

    let start = std::time::Instant::now();
    let result = manager
        .call_tool("127.0.0.1", "fast_tool", json!({}), &capability)
        .await
        .expect("session expiry should recover quickly");

    let elapsed = start.elapsed();

    assert_eq!(result["fast_recovery"], json!(true));
    assert!(
        elapsed < Duration::from_secs(5),
        "session expiry recovery should be immediate (no backoff), took {:?}",
        elapsed
    );

    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p simulacra-mcp s024`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/simulacra-mcp/tests/s024_streamable_http_red.rs
git commit -m "test(mcp): reconnection and session expiry timing tests [S024]"
```

---

### Task 7: Observability span attributes and non-fallback error tests

**Files:**
- Modify: `crates/simulacra-mcp/tests/s024_streamable_http_red.rs`
- Modify: `crates/simulacra-mcp/src/lib.rs` (add `simulacra.mcp.response_type` to call_tool span)

Add the remaining observability assertions and the 401/403 non-fallback test.

- [ ] **Step 1: Write the failing tests**

Add to `crates/simulacra-mcp/tests/s024_streamable_http_red.rs`:

```rust
// ── S024 Assertion: 401/403 does NOT fall back to SSE ──

#[tokio::test]
async fn autodetect_does_not_fall_back_on_401_or_403() {
    // Server that returns 403 on POST
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let sse_attempts = Arc::new(AtomicUsize::new(0));

    let stop_thread = Arc::clone(&stop);
    let sse_attempts_thread = Arc::clone(&sse_attempts);

    let handle = thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

                    let request = match read_http_request(&mut stream) {
                        Some(r) => r,
                        None => continue,
                    };

                    if request.starts_with("POST ") {
                        let response = http_response_status(403, "Forbidden");
                        let _ = stream.write_all(response.as_bytes());
                    } else if request.starts_with("GET ") {
                        sse_attempts_thread.fetch_add(1, Ordering::SeqCst);
                        let response = http_response_status(404, "Not Found");
                        let _ = stream.write_all(response.as_bytes());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let mut manager = McpManager::new();
    manager
        .connect(&format!("http://{addr}/mcp"), None)
        .await
        .unwrap();

    // list_tools triggers handshake — should fail without falling back to SSE
    let tools = manager.list_tools().await;
    assert!(tools.is_empty(), "should get no tools on auth failure");
    assert_eq!(
        sse_attempts.load(Ordering::SeqCst),
        0,
        "should NOT fall back to SSE on 403"
    );

    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}

// ── S024 Assertion: Existing S008 configs with transport=sse work unchanged ──

#[tokio::test]
async fn existing_sse_config_works_unchanged() {
    // This is essentially the same as the existing S008 SSE tests,
    // but using the new connect() method with transport="sse".
    let tools_body = json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [{
                "name": "legacy_sse_tool",
                "description": "Legacy SSE tool",
                "inputSchema": { "type": "object" }
            }]
        }
    })
    .to_string();

    let server = spawn_fallback_test_server(&tools_body);

    let mut manager = McpManager::new();
    manager
        .connect(&server.url(), Some("sse"))
        .await
        .unwrap();

    let tools = manager.list_tools().await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "legacy_sse_tool");
}
```

- [ ] **Step 2: Add `simulacra.mcp.response_type` span attribute to `call_tool`**

In `crates/simulacra-mcp/src/lib.rs`, update the `call_tool` method's span to include a recordable `simulacra.mcp.response_type` field. The field is recorded in `dispatch_tool_call` after the response content-type is known:

In `call_tool`, update the span creation:

```rust
let span = tracing::info_span!(
    "execute_tool",
    gen_ai.operation.name = "execute_tool",
    simulacra.tool.name = tool,
    simulacra.tool.source = %source,
    simulacra.mcp.response_type = tracing::field::Empty,
);
let _guard = span.enter();
```

In `dispatch_tool_call`, after determining the content type, record it:

```rust
if content_type.contains("text/event-stream") {
    tracing::Span::current().record("simulacra.mcp.response_type", "sse_stream");
    self.parse_sse_tool_response(response, server).await
} else {
    tracing::Span::current().record("simulacra.mcp.response_type", "json");
    // ... existing JSON parsing
}
```

- [ ] **Step 3: Run all tests**

Run: `cargo test -p simulacra-mcp`
Expected: PASS — all S008 and S024 tests pass

- [ ] **Step 4: Run mechanical gate**

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

All four must pass.

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-mcp/src/lib.rs crates/simulacra-mcp/tests/s024_streamable_http_red.rs
git commit -m "feat(mcp): observability attributes, non-fallback error handling [S024]"
```

---

## Self-Review

**Spec coverage check:**

| Spec Section | Task |
|---|---|
| Auto-detection (behaviors 1-5) | Task 3 |
| Streamable HTTP handshake (behaviors 6-11) | Task 3 |
| Session management (behaviors 12-16) | Task 4 |
| Tool call responses (behaviors 17-23) | Tasks 4-5 |
| Reconnection (behaviors 24-27) | Tasks 2, 6 |
| Config (behaviors 28-30) | Task 1 |
| What doesn't change (behaviors 31-37) | Verified by existing S008 tests passing |
| Observability assertions | Tasks 3, 4, 7 |

**Placeholder scan:** No TBD/TODO found. All code blocks are complete.

**Type consistency:**
- `TransportMode` enum: defined in Task 2, used consistently in Tasks 3-7
- `perform_streamable_http_handshake` returns `(Vec<McpToolSchema>, Option<String>)`: defined in Task 3, consumed in Task 3
- `parse_sse_tool_response` / `parse_next_sse_event`: defined in Task 4, tested in Task 5
- `session_expired` counter: added to `McpMeters` in Task 4, used in Task 4
- `connect()` method: defined in Task 2, used in all S024 tests
- `configured_transport` field: defined in Task 2, dispatched in Task 3
