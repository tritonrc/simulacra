//! Toy MCP server for integration testing.
//!
//! Supports both transport modes:
//!   - Streamable HTTP (2025-03-26): POST /mcp for everything
//!   - Legacy SSE (2024-11-05): GET /sse for endpoint discovery, POST /mcp-rpc for JSON-RPC
//!
//! Usage:
//!   cargo run -p simulacra-mcp-test-server -- --port 9800 --mode both
//!   cargo run -p simulacra-mcp-test-server -- --mode streamable   # streamable HTTP only
//!   cargo run -p simulacra-mcp-test-server -- --mode legacy-sse   # legacy SSE only

use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(
    name = "mcp-test-server",
    about = "Toy MCP server for integration testing"
)]
struct Cli {
    /// Port to listen on
    #[arg(short, long, default_value = "9800")]
    port: u16,

    /// Transport mode: "both", "streamable", "legacy-sse"
    #[arg(short, long, default_value = "both")]
    mode: String,

    /// Enable SSE streaming responses for tool calls (sends progress events)
    #[arg(long)]
    stream_tool_responses: bool,
}

#[derive(Clone)]
struct ServerState {
    mode: String,
    stream_tool_responses: bool,
    sessions: Arc<Mutex<HashMap<String, SessionInfo>>>,
}

#[derive(Clone)]
struct SessionInfo {
    #[allow(dead_code)]
    created_at: std::time::Instant,
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

impl JsonRpcResponse {
    fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<Value>, code: i64, message: &str) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(json!({ "code": code, "message": message })),
        }
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "echo",
            "description": "Echo back the input text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Text to echo back" }
                },
                "required": ["text"]
            }
        },
        {
            "name": "add",
            "description": "Add two numbers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "a": { "type": "number", "description": "First number" },
                    "b": { "type": "number", "description": "Second number" }
                },
                "required": ["a", "b"]
            }
        },
        {
            "name": "get_time",
            "description": "Get the current server time as ISO 8601.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "slow_task",
            "description": "A deliberately slow task that reports progress. Takes 3 seconds.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "steps": { "type": "integer", "description": "Number of progress steps (default 3)" }
                }
            }
        }
    ])
}

async fn handle_tool_call(name: &str, arguments: &Value) -> Result<Value, String> {
    match name {
        "echo" => {
            let text = arguments.get("text").and_then(|v| v.as_str()).unwrap_or("");
            Ok(json!({ "text": text }))
        }
        "add" => {
            let a = arguments
                .get("a")
                .and_then(|v| v.as_f64())
                .ok_or("missing parameter 'a'")?;
            let b = arguments
                .get("b")
                .and_then(|v| v.as_f64())
                .ok_or("missing parameter 'b'")?;
            Ok(json!({ "sum": a + b }))
        }
        "get_time" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            Ok(json!({ "unix_timestamp": now }))
        }
        "slow_task" => {
            let steps = arguments.get("steps").and_then(|v| v.as_u64()).unwrap_or(3) as u32;
            let delay = std::time::Duration::from_millis(1000 / steps.max(1) as u64);
            for _ in 0..steps {
                tokio::time::sleep(delay).await;
            }
            Ok(json!({ "completed": true, "steps": steps }))
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

fn handle_jsonrpc(req: &JsonRpcRequest) -> Option<JsonRpcResponse> {
    match req.method.as_str() {
        "initialize" => {
            let client_version = req
                .params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            tracing::info!(client_version = client_version, "initialize");

            let server_version = if client_version == "2025-03-26" {
                "2025-03-26"
            } else {
                "2024-11-05"
            };

            Some(JsonRpcResponse::success(
                req.id.clone(),
                json!({
                    "protocolVersion": server_version,
                    "serverInfo": {
                        "name": "simulacra-mcp-test-server",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {
                        "tools": { "listChanged": false }
                    }
                }),
            ))
        }
        "notifications/initialized" => {
            tracing::info!("notifications/initialized received");
            None // notifications don't get responses
        }
        "tools/list" => {
            tracing::info!("tools/list");
            Some(JsonRpcResponse::success(
                req.id.clone(),
                json!({ "tools": tool_definitions() }),
            ))
        }
        "tools/call" => {
            // This is handled specially for streaming — shouldn't reach here in streaming mode
            tracing::info!(
                tool = req
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?"),
                "tools/call (sync)"
            );
            // For sync: we'd need to block on the async handle_tool_call, but since
            // this path is only used in non-streaming mode, we handle it at the caller.
            None // handled by caller
        }
        _ => {
            tracing::warn!(method = req.method.as_str(), "unknown method");
            Some(JsonRpcResponse::error(
                req.id.clone(),
                -32601,
                &format!("method not found: {}", req.method),
            ))
        }
    }
}

/// Parse an HTTP request from a buffered reader. Returns (method, path, headers, body).
async fn read_http_request(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Option<(String, String, HashMap<String, String>, String)> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.ok()? == 0 {
        return None;
    }

    let trimmed = request_line.trim();
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let method = parts[0].to_string();
    let path = parts[1].to_string();

    // Read headers
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            break;
        }
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.insert(name.trim().to_lowercase(), value.trim().to_string());
        }
    }

    // Read body based on content-length
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await.ok()?;
    }

    Some((
        method,
        path,
        headers,
        String::from_utf8_lossy(&body).to_string(),
    ))
}

fn http_response(status: u16, reason: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!("Content-Length: {}\r\n", body.len()));
    response.push_str("Connection: keep-alive\r\n");
    response.push_str("\r\n");
    response.push_str(body);
    response.into_bytes()
}

fn json_response(status: u16, body: &str, extra_headers: &[(&str, &str)]) -> Vec<u8> {
    let mut headers = vec![("Content-Type", "application/json")];
    headers.extend_from_slice(extra_headers);
    http_response(status, "OK", &headers, body)
}

async fn handle_connection(stream: tokio::net::TcpStream, state: ServerState) {
    let _peer = stream.peer_addr().ok();
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        let (method, path, headers, body) = match read_http_request(&mut reader).await {
            Some(req) => req,
            None => break,
        };

        tracing::debug!(method = %method, path = %path, "request");

        // ── Legacy SSE: GET /sse → endpoint discovery ──
        if method == "GET" && path == "/sse" {
            if state.mode == "streamable" {
                // Streamable-only mode: reject SSE
                let resp = http_response(405, "Method Not Allowed", &[], "");
                let _ = write_half.write_all(&resp).await;
                break;
            }

            tracing::info!("SSE connection opened, sending endpoint discovery");

            let sse_header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
            let _ = write_half.write_all(sse_header.as_bytes()).await;

            // Send endpoint discovery event
            let _ = write_half
                .write_all(b"event: endpoint\ndata: /mcp-rpc\n\n")
                .await;

            // Keep connection alive with periodic comments
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                if write_half.write_all(b": keepalive\n\n").await.is_err() {
                    break;
                }
            }
            break;
        }

        // ── Streamable HTTP: POST /mcp ──
        if method == "POST" && path == "/mcp" {
            if state.mode == "legacy-sse" {
                // Legacy-only mode: reject POST to /mcp
                let resp = http_response(405, "Method Not Allowed", &[], "");
                let _ = write_half.write_all(&resp).await;
                continue;
            }

            // Validate/create session
            let session_id = headers.get("mcp-session-id").cloned();
            let new_session_id = if session_id.is_none() {
                // First request (initialize) — create a session
                let sid = uuid::Uuid::new_v4().to_string();
                state.sessions.lock().await.insert(
                    sid.clone(),
                    SessionInfo {
                        created_at: std::time::Instant::now(),
                    },
                );
                Some(sid)
            } else if let Some(ref sid) = session_id {
                let sessions = state.sessions.lock().await;
                if sessions.contains_key(sid) {
                    Some(sid.clone())
                } else {
                    // Unknown session → 404
                    tracing::warn!(session_id = %sid, "unknown session, returning 404");
                    let resp = http_response(404, "Not Found", &[], "");
                    let _ = write_half.write_all(&resp).await;
                    continue;
                }
            } else {
                None
            };

            let session_header = new_session_id.as_deref().unwrap_or("");

            // Parse JSON-RPC body
            let rpc_req: JsonRpcRequest = match serde_json::from_str(&body) {
                Ok(r) => r,
                Err(e) => {
                    let err = JsonRpcResponse::error(None, -32700, &format!("parse error: {e}"));
                    let resp_body = serde_json::to_string(&err).unwrap();
                    let resp = json_response(400, &resp_body, &[]);
                    let _ = write_half.write_all(&resp).await;
                    continue;
                }
            };

            // Handle tools/call specially for streaming
            if rpc_req.method == "tools/call" {
                let tool_name = rpc_req
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let arguments = rpc_req
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(json!({}));

                tracing::info!(tool = %tool_name, "tools/call");

                let accepts_sse = headers
                    .get("accept")
                    .map(|a| a.contains("text/event-stream"))
                    .unwrap_or(false);

                if state.stream_tool_responses && accepts_sse && tool_name == "slow_task" {
                    // SSE streaming response for slow_task
                    let sse_header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nMcp-Session-Id: {session_header}\r\nConnection: keep-alive\r\n\r\n"
                    );
                    let _ = write_half.write_all(sse_header.as_bytes()).await;

                    let steps = arguments.get("steps").and_then(|v| v.as_u64()).unwrap_or(3) as u32;
                    let delay = std::time::Duration::from_millis(1000 / steps.max(1) as u64);

                    for step in 1..=steps {
                        tokio::time::sleep(delay).await;

                        let progress = json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": 1,
                                "progress": step,
                                "total": steps
                            }
                        });
                        let event = format!("data: {}\n\n", progress);
                        if write_half.write_all(event.as_bytes()).await.is_err() {
                            break;
                        }
                        tracing::debug!(step = step, total = steps, "progress");
                    }

                    // Final result
                    let result = JsonRpcResponse::success(
                        rpc_req.id.clone(),
                        json!({
                            "content": [{
                                "type": "text",
                                "text": format!("slow_task completed in {steps} steps")
                            }]
                        }),
                    );
                    let event = format!("data: {}\n\n", serde_json::to_string(&result).unwrap());
                    let _ = write_half.write_all(event.as_bytes()).await;
                    break; // Close stream after result
                }

                // Non-streaming tool call
                let result = match handle_tool_call(&tool_name, &arguments).await {
                    Ok(value) => JsonRpcResponse::success(
                        rpc_req.id.clone(),
                        json!({
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string(&value).unwrap()
                            }]
                        }),
                    ),
                    Err(msg) => JsonRpcResponse::error(rpc_req.id.clone(), -32000, &msg),
                };

                let resp_body = serde_json::to_string(&result).unwrap();
                let mut extra = vec![];
                if !session_header.is_empty() {
                    extra.push(("Mcp-Session-Id", session_header));
                }
                let resp = json_response(200, &resp_body, &extra);
                let _ = write_half.write_all(&resp).await;
                continue;
            }

            // All other JSON-RPC methods
            if let Some(response) = handle_jsonrpc(&rpc_req) {
                let resp_body = serde_json::to_string(&response).unwrap();
                let mut extra = vec![];
                if !session_header.is_empty() {
                    extra.push(("Mcp-Session-Id", session_header));
                }
                let resp = json_response(200, &resp_body, &extra);
                let _ = write_half.write_all(&resp).await;
            } else {
                // Notification — 202 Accepted
                let mut extra_headers = vec![];
                if !session_header.is_empty() {
                    extra_headers.push(("Mcp-Session-Id", session_header));
                }
                let resp = http_response(202, "Accepted", &extra_headers, "");
                let _ = write_half.write_all(&resp).await;
            }
            continue;
        }

        // ── Legacy SSE: POST /mcp-rpc ──
        if method == "POST" && path == "/mcp-rpc" {
            if state.mode == "streamable" {
                let resp = http_response(404, "Not Found", &[], "");
                let _ = write_half.write_all(&resp).await;
                continue;
            }

            let rpc_req: JsonRpcRequest = match serde_json::from_str(&body) {
                Ok(r) => r,
                Err(e) => {
                    let err = JsonRpcResponse::error(None, -32700, &format!("parse error: {e}"));
                    let resp_body = serde_json::to_string(&err).unwrap();
                    let resp = json_response(400, &resp_body, &[]);
                    let _ = write_half.write_all(&resp).await;
                    continue;
                }
            };

            // Handle tools/call
            if rpc_req.method == "tools/call" {
                let tool_name = rpc_req
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let arguments = rpc_req
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(json!({}));

                tracing::info!(tool = %tool_name, "tools/call (legacy)");

                let result = match handle_tool_call(&tool_name, &arguments).await {
                    Ok(value) => JsonRpcResponse::success(
                        rpc_req.id.clone(),
                        json!({
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string(&value).unwrap()
                            }]
                        }),
                    ),
                    Err(msg) => JsonRpcResponse::error(rpc_req.id.clone(), -32000, &msg),
                };

                let resp_body = serde_json::to_string(&result).unwrap();
                let resp = json_response(200, &resp_body, &[]);
                let _ = write_half.write_all(&resp).await;
                continue;
            }

            // All other methods
            if let Some(response) = handle_jsonrpc(&rpc_req) {
                let resp_body = serde_json::to_string(&response).unwrap();
                let resp = json_response(200, &resp_body, &[]);
                let _ = write_half.write_all(&resp).await;
            } else {
                let resp = http_response(202, "Accepted", &[], "");
                let _ = write_half.write_all(&resp).await;
            }
            continue;
        }

        // Unknown path
        let resp = http_response(404, "Not Found", &[], "");
        let _ = write_half.write_all(&resp).await;
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let state = ServerState {
        mode: cli.mode.clone(),
        stream_tool_responses: cli.stream_tool_responses,
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };

    let addr = format!("127.0.0.1:{}", cli.port);
    let listener = TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind to {addr}: {e}"));

    tracing::info!(
        addr = %addr,
        mode = %cli.mode,
        stream_tool_responses = cli.stream_tool_responses,
        "MCP test server listening"
    );

    match cli.mode.as_str() {
        "both" => {
            tracing::info!("  Streamable HTTP: POST http://{addr}/mcp");
            tracing::info!("  Legacy SSE:      GET  http://{addr}/sse");
            tracing::info!("  Legacy RPC:      POST http://{addr}/mcp-rpc");
        }
        "streamable" => {
            tracing::info!("  Streamable HTTP: POST http://{addr}/mcp");
        }
        "legacy-sse" => {
            tracing::info!("  Legacy SSE:      GET  http://{addr}/sse");
            tracing::info!("  Legacy RPC:      POST http://{addr}/mcp-rpc");
        }
        _ => {
            tracing::error!(
                "unknown mode: {}. Use 'both', 'streamable', or 'legacy-sse'",
                cli.mode
            );
            std::process::exit(1);
        }
    }

    tracing::info!("Tools: echo, add, get_time, slow_task");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };

        tracing::debug!(peer = %peer, "connection accepted");
        let state = state.clone();
        tokio::spawn(async move {
            handle_connection(stream, state).await;
        });
    }
}
