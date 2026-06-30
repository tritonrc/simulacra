//! Simulacra MCP (Model Context Protocol) crate.
//!
//! Manages connections to external MCP servers over SSE and HTTP
//! transports and exposes their tools as `ToolDefinition` values.
//!
//! Per R002: this crate MUST NOT use `std::process::Command` or
//! `tokio::process` — all communication happens over network transports.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use serde::{Deserialize, Serialize};
pub use simulacra_types::ToolDefinition;
use simulacra_types::{
    AgentId, CapabilityToken, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind,
    JournalStorage,
};

// ── S041 simulacra:mcp/server bindings ───────────────────────────────
//
// The WIT world that real WASM MCP server modules target. Includes the
// `simulacra:mcp/http.fetch` host import — the seam through which a WASM
// module's outbound HTTP runs through Simulacra's allowlist + governance
// hooks + journal.
#[cfg(feature = "wasm")]
mod wit_server {
    wasmtime::component::bindgen!({
        world: "server",
        path: "wit/simulacra-mcp-server.wit",
    });
}

// ── OTel meters ──────────────────────────────────────────────────

/// Lazily-initialized OTel meter instruments for MCP tool calls.
/// Created on first use so they pick up the global MeterProvider
/// (which may not be set at construction time).
struct McpMeters {
    tool_duration: Histogram<f64>,
    /// S010: `simulacra.mcp.calls` counter with labels `server`, `tool`.
    calls: Counter<u64>,
    tool_errors: Counter<u64>,
    /// S024: `simulacra.mcp.session_expired` counter with label `server`.
    session_expired: Counter<u64>,
    /// S041 §Observability: `simulacra.wasm.fuel_consumed` histogram with
    /// labels `module` and `tool`. Recorded per WASM MCP tool call —
    /// the OTel meter bridge picks it up regardless of the
    /// tracing-fields histogram convention being supported downstream.
    #[cfg(feature = "wasm")]
    wasm_fuel_consumed: Histogram<u64>,
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
                #[cfg(feature = "wasm")]
                wasm_fuel_consumed: meter
                    .u64_histogram("simulacra.wasm.fuel_consumed")
                    .with_description("Wasmtime fuel consumed per WASM MCP tool call")
                    .build(),
            }
        })
    }
}

/// Errors from MCP operations.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("protocol error: {0}")]
    ProtocolError(String),

    #[error("transport error: {0}")]
    TransportError(String),

    #[error("capability denied: {0}")]
    CapabilityDenied(String),
}

/// Raw tool description received from an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct McpToolSchema {
    name: String,
    description: String,
    #[serde(alias = "inputSchema")]
    input_schema: serde_json::Value,
}

/// Transport mode for an MCP connection, determined during handshake.
#[derive(Debug)]
enum TransportMode {
    /// 2025-03-26 streamable HTTP — single endpoint, optional session ID.
    StreamableHttp { session_id: Option<String> },
    #[cfg(feature = "wasm")]
    Wasm {
        #[allow(dead_code)]
        module_id: String,
    },
    /// 2024-11-05 legacy SSE — discovered POST endpoint, persistent stream.
    LegacySse {
        post_endpoint: String,
        #[allow(dead_code)]
        sse_handle: tokio::task::JoinHandle<()>,
    },
    /// 2024-11-05 legacy HTTP — plain request/response (no SSE).
    #[allow(dead_code)]
    LegacyHttp,
}

/// State for a single connected MCP server.
#[derive(Debug)]
struct McpConnection {
    #[allow(dead_code)]
    server_name: String,
    #[allow(dead_code)]
    url: String,
    headers: Vec<(String, String)>,
    tools: Vec<McpToolSchema>,
    /// Whether the MCP handshake (initialize + tools/list) has been performed.
    handshake_done: bool,
    /// Whether this connection has previously completed a successful handshake.
    /// Used to decide whether to attempt reconnection on failure.
    was_connected: bool,
    /// Transport mode, determined during handshake. None before first handshake.
    transport_mode: Option<TransportMode>,
    /// Configured transport preference from simulacra.toml.
    /// None = auto-detect, Some("sse") = legacy SSE, Some("http") = streamable HTTP.
    configured_transport: Option<String>,
}

impl McpConnection {
    fn new(
        server_name: String,
        url: String,
        configured_transport: Option<String>,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            server_name,
            url,
            headers,
            tools: Vec::new(),
            handshake_done: false,
            was_connected: false,
            transport_mode: None,
            configured_transport,
        }
    }
}

fn apply_connection_headers(
    mut builder: reqwest::RequestBuilder,
    headers: &[(String, String)],
) -> reqwest::RequestBuilder {
    if !headers.is_empty() {
        tracing::debug!(
            connection.headers = %redact_headers_for_log(headers),
            "applying connection headers to MCP request"
        );
    }

    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
}

/// Return a log-safe display string for connection headers, masking secret values.
pub fn redact_headers_for_log(headers: &[(String, String)]) -> String {
    let parts = headers
        .iter()
        .map(|(name, value)| {
            let lower_name = name.to_ascii_lowercase();
            let redact_value = matches!(
                lower_name.as_str(),
                "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
            ) || lower_name.starts_with("x-mcp-")
                || lower_name.starts_with("x-api")
                || lower_name.ends_with("-token")
                || lower_name.ends_with("-key")
                || lower_name.ends_with("-secret")
                || lower_name.ends_with("-auth");
            let display_value = if redact_value { "***" } else { value.as_str() };
            format!("{name}: {display_value}")
        })
        .collect::<Vec<_>>()
        .join(", ");

    format!("[{parts}]")
}

/// Manager for MCP server connections.
///
/// Holds active connections and aggregates tool definitions from
/// all connected servers.
pub struct McpManager {
    connections: HashMap<String, McpConnection>,
    /// Optional Journal storage for recording ToolCall entries.
    #[allow(dead_code)]
    journal: Option<Arc<dyn JournalStorage>>,
    /// Agent ID for journal entries.
    #[allow(dead_code)]
    agent_id: AgentId,
    /// Base delay in milliseconds for reconnection exponential backoff.
    reconnect_base_delay_ms: u64,
    /// Shared agent-level fuel budget for WASM MCP calls. `None` means
    /// "unlimited budget"; `Some(arc)` carries the live remaining counter
    /// that `WasmTool::reserve_agent_fuel` decrements per call.
    agent_fuel_remaining: Option<Arc<AtomicU64>>,
    /// Test hook: counter incremented every time the runtime instantiates
    /// a wasmtime component to dispatch a WASM MCP tool call. The
    /// agent-fuel-exhausted path must short-circuit before this counter
    /// ticks (see `wasm_mcp_transport::call_tool_when_agent_fuel_budget_
    /// exhausted_fails_without_instantiating_component`).
    instantiation_recorder: Option<Arc<AtomicUsize>>,
    /// Loaded WASM MCP modules keyed by server name. Populated by
    /// `connect_wasm_module`. Each entry owns the compiled component and
    /// the discovered `(ToolDefinition, WasmTool)` pairs.
    #[cfg(feature = "wasm")]
    wasm_modules: HashMap<String, WasmMcpModule>,
}

impl McpManager {
    /// Create a new MCP manager with no connections.
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            journal: None,
            agent_id: AgentId(String::new()),
            reconnect_base_delay_ms: 1000,
            agent_fuel_remaining: None,
            instantiation_recorder: None,
            #[cfg(feature = "wasm")]
            wasm_modules: HashMap::new(),
        }
    }

    /// Create a new MCP manager with journal support.
    #[allow(dead_code)]
    pub fn with_journal(journal: Arc<dyn JournalStorage>, agent_id: AgentId) -> Self {
        Self {
            connections: HashMap::new(),
            journal: Some(journal),
            agent_id,
            reconnect_base_delay_ms: 1000,
            agent_fuel_remaining: None,
            instantiation_recorder: None,
            #[cfg(feature = "wasm")]
            wasm_modules: HashMap::new(),
        }
    }

    /// Override the base delay for reconnection backoff (milliseconds).
    ///
    /// Default is 1000ms. Useful for tests that need faster retries.
    pub fn set_reconnect_base_delay_ms(&mut self, ms: u64) {
        self.reconnect_base_delay_ms = ms;
    }

    /// Set the agent-level fuel budget for WASM MCP calls.
    ///
    /// Replaces any previous budget with a fresh shared counter seeded at
    /// `fuel`. A budget of `0` is **exhausted** (every subsequent WASM MCP
    /// call fails with `fuel exhausted` before any wasmtime instantiation),
    /// matching the `simulacra_wasm::WasmTool` semantics.
    pub fn set_agent_fuel_budget(&mut self, fuel: u64) {
        self.agent_fuel_remaining = Some(Arc::new(AtomicU64::new(fuel)));
    }

    /// Inspect the agent-level fuel budget remaining for WASM MCP calls.
    ///
    /// Reads the live counter shared with `WasmTool` so callers see the
    /// post-call balance, not the originally-configured budget.
    pub fn agent_fuel_budget_remaining(&self) -> Option<u64> {
        self.agent_fuel_remaining
            .as_ref()
            .map(|arc| arc.load(Ordering::Acquire))
    }

    /// Connect to an MCP server via Server-Sent Events.
    ///
    /// Registers the SSE endpoint for lazy connection. The actual
    /// SSE stream and MCP handshake are deferred until first use
    /// (via `list_tools` or `call_tool`).
    pub async fn connect_sse(&mut self, url: &str) -> Result<(), McpError> {
        let parsed = url::Url::parse(url).map_err(|e| {
            let server = url;
            let error = e.to_string();
            tracing::warn!(
                server = server,
                error = %error,
                "WARN: MCP connection failure"
            );
            McpError::ConnectionFailed(format!("sse connection to {url} failed: {e}"))
        })?;

        let server_name = parsed.host_str().unwrap_or("unknown").to_string();

        self.connections.insert(
            server_name.clone(),
            McpConnection::new(
                server_name,
                url.to_string(),
                Some("sse".to_string()),
                Vec::new(),
            ),
        );

        Ok(())
    }

    /// Connect to an MCP server via HTTP request/response.
    ///
    /// Registers the server URL without performing any network I/O.
    /// The MCP handshake (initialize + tools/list) is deferred until
    /// first use via `list_tools` or `call_tool`.
    pub async fn connect_http(&mut self, url: &str) -> Result<(), McpError> {
        let parsed = url::Url::parse(url).map_err(|e| {
            let server = url;
            let error = e.to_string();
            tracing::warn!(
                server = server,
                error = %error,
                "WARN: MCP connection failure"
            );
            McpError::ConnectionFailed(format!("http connection to {url} failed: {e}"))
        })?;

        let server_name = parsed.host_str().unwrap_or("unknown").to_string();

        self.connections.insert(
            server_name.clone(),
            McpConnection::new(
                server_name,
                url.to_string(),
                Some("http".to_string()),
                Vec::new(),
            ),
        );

        Ok(())
    }

    /// Register an MCP server connection with auto-detect or explicit transport.
    ///
    /// If `transport` is `None`, empty, or `Some("auto")`, the transport mode
    /// will be auto-detected during the first handshake. If `Some("sse")`,
    /// forces legacy SSE.
    /// If `Some("http")`, forces streamable HTTP with no fallback.
    ///
    /// WARNING 4: Only `None`, `Some("")`, `Some("auto")`, `Some("sse")`,
    /// and `Some("http")` are accepted. Any other transport string is rejected with a
    /// ConnectionFailed error rather than silently falling through to
    /// auto-detect.
    pub async fn connect(&mut self, url: &str, transport: Option<&str>) -> Result<(), McpError> {
        let transport = normalize_transport(transport)?;

        let parsed = url::Url::parse(url).map_err(|e| {
            let error = e.to_string();
            tracing::warn!(server = url, error = %error, "WARN: MCP connection failure");
            McpError::ConnectionFailed(format!("connection to {url} failed: {e}"))
        })?;

        let server_name = parsed.host_str().unwrap_or("unknown").to_string();

        self.connections.insert(
            server_name.clone(),
            McpConnection::new(server_name, url.to_string(), transport, Vec::new()),
        );

        Ok(())
    }

    /// Ensure that the MCP handshake has been performed for all registered
    /// connections. For connections that have not yet done the handshake,
    /// this sends the initialize + tools/list JSON-RPC requests and starts
    /// SSE background tasks as needed.
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

    /// Ensure that a specific server connection has completed its handshake.
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

    /// Dispatch the handshake for a server based on its configured transport.
    ///
    /// `configured_transport == Some("sse")` → SSE handshake (no auto-detect).
    /// `configured_transport == Some("http")` → streamable HTTP only (no fallback).
    /// `configured_transport == None` → auto-detect: try streamable HTTP first,
    ///   fall back to legacy SSE on 404/405.
    async fn handshake_server(&mut self, key: &str) {
        let conn = match self.connections.get(key) {
            Some(c) => c,
            None => return,
        };

        let configured = conn.configured_transport.as_deref();

        // WARNING 4: Only accept whitelisted transport strings. Unknown
        // values should never reach here because `normalize_transport`
        // rejects them at registration, but refuse to guess here just in
        // case a caller constructed an McpConnection by other means.
        match configured {
            None | Some("") => {
                self.auto_detect_handshake(key).await;
            }
            Some("sse") => {
                // Forced legacy SSE — skip auto-detect.
                self.perform_sse_handshake(key).await;
            }
            Some("http") => {
                self.forced_http_handshake(key).await;
            }
            Some(other) => {
                tracing::warn!(
                    server = %key,
                    transport = %other,
                    "MCP handshake aborted: unknown configured transport (expected \"sse\" or \"http\")"
                );
            }
        }
    }

    /// Forced streamable HTTP handshake — no fallback.
    async fn forced_http_handshake(&mut self, key: &str) {
        let (url, headers) = match self.connections.get(key) {
            Some(c) => (c.url.clone(), c.headers.clone()),
            None => return,
        };

        let span = tracing::info_span!(
            "simulacra_mcp_handshake",
            simulacra.mcp.transport_mode = "streamable_http",
            simulacra.mcp.protocol_version = "2025-03-26",
            simulacra.mcp.session_id = tracing::field::Empty,
        );

        match self.perform_streamable_http_handshake(&url, &headers).await {
            Ok((tools, session_id)) => {
                if let Some(ref sid) = session_id {
                    span.record("simulacra.mcp.session_id", sid.as_str());
                }
                if let Some(conn) = self.connections.get_mut(key) {
                    conn.tools = tools;
                    conn.handshake_done = true;
                    conn.was_connected = true;
                    conn.transport_mode = Some(TransportMode::StreamableHttp { session_id });
                }
            }
            Err(e) => {
                let server_name = self
                    .connections
                    .get(key)
                    .map(|c| c.server_name.clone())
                    .unwrap_or_else(|| key.to_string());
                tracing::warn!(
                    server = %server_name,
                    error = %e,
                    "MCP streamable HTTP handshake failure"
                );
                // Forced "http" failure is final — do NOT set handshake_done
                // so the connection remains unusable but retryable.
                if let Some(conn) = self.connections.get_mut(key) {
                    conn.tools = Vec::new();
                    // handshake_done stays false, was_connected stays false.
                }
            }
        }
    }

    /// Auto-detect: try streamable HTTP first, fall back to legacy SSE on 404/405.
    async fn auto_detect_handshake(&mut self, key: &str) {
        let (url, headers) = match self.connections.get(key) {
            Some(c) => (c.url.clone(), c.headers.clone()),
            None => return,
        };

        let span = tracing::info_span!(
            "simulacra_mcp_handshake",
            simulacra.mcp.transport_mode = tracing::field::Empty,
            simulacra.mcp.protocol_version = tracing::field::Empty,
            simulacra.mcp.session_id = tracing::field::Empty,
        );

        match self.perform_streamable_http_handshake(&url, &headers).await {
            Ok((tools, session_id)) => {
                span.record("simulacra.mcp.transport_mode", "streamable_http");
                span.record("simulacra.mcp.protocol_version", "2025-03-26");
                if let Some(ref sid) = session_id {
                    span.record("simulacra.mcp.session_id", sid.as_str());
                }
                if let Some(conn) = self.connections.get_mut(key) {
                    conn.tools = tools;
                    conn.handshake_done = true;
                    conn.was_connected = true;
                    conn.transport_mode = Some(TransportMode::StreamableHttp { session_id });
                }
            }
            Err(McpError::TransportError(ref msg))
                if msg.contains("404") || msg.contains("405") =>
            {
                let server_name = self
                    .connections
                    .get(key)
                    .map(|c| c.server_name.clone())
                    .unwrap_or_else(|| key.to_string());
                tracing::info!(
                    server = %server_name,
                    streamable_http_error = %msg,
                    "Auto-detect: streamable HTTP returned 404/405, falling back to legacy SSE"
                );
                // Fall back to legacy SSE.
                self.perform_sse_handshake(key).await;
                // Record transport mode on span after SSE handshake.
                span.record("simulacra.mcp.transport_mode", "legacy_sse");
                span.record("simulacra.mcp.protocol_version", "2024-11-05");
            }
            Err(e) => {
                // Non-fallback error (auth, 5xx, network) — do not try SSE.
                let server_name = self
                    .connections
                    .get(key)
                    .map(|c| c.server_name.clone())
                    .unwrap_or_else(|| key.to_string());
                tracing::warn!(
                    server = %server_name,
                    error = %e,
                    "MCP connection failure (not eligible for SSE fallback)"
                );
                // Do NOT set handshake_done — connection stays retryable.
                if let Some(conn) = self.connections.get_mut(key) {
                    conn.tools = Vec::new();
                    // handshake_done stays false, was_connected stays false.
                }
            }
        }
    }

    /// Perform the 2025-03-26 streamable HTTP handshake.
    ///
    /// POSTs `InitializeRequest` to the server URL with
    /// `Accept: application/json, text/event-stream` and protocol version
    /// `2025-03-26`. Extracts `Mcp-Session-Id` from response headers.
    /// Sends `notifications/initialized` and `tools/list`.
    ///
    /// Returns `(tools, session_id)` on success.
    /// Returns `McpError::TransportError` with "404" or "405" when the server
    /// responds with those status codes (caller should fall back to SSE).
    /// Returns `McpError::ConnectionFailed` for auth errors (401, 403) or
    /// server errors (5xx) — these are not fallback-eligible.
    async fn perform_streamable_http_handshake(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<(Vec<McpToolSchema>, Option<String>), McpError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| McpError::TransportError(e.to_string()))?;

        // Step 1: POST initialize with streamable HTTP headers.
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

        let init_request = client
            .post(url)
            .header("Accept", "application/json, text/event-stream")
            .json(&initialize_request);
        let init_request = apply_connection_headers(init_request, headers);
        let init_response = init_request
            .send()
            .await
            .map_err(|e| McpError::ConnectionFailed(e.to_string()))?;

        let status = init_response.status().as_u16();

        // Check status codes for fallback eligibility.
        match status {
            404 => {
                return Err(McpError::TransportError(
                    "server returned 404 for streamable HTTP initialize".to_string(),
                ));
            }
            405 => {
                return Err(McpError::TransportError(
                    "server returned 405 for streamable HTTP initialize".to_string(),
                ));
            }
            401 | 403 => {
                return Err(McpError::ConnectionFailed(format!(
                    "server returned {status} (auth/permission error)"
                )));
            }
            s if s >= 500 => {
                return Err(McpError::ConnectionFailed(format!(
                    "server returned {status} (server error)"
                )));
            }
            s if s >= 400 => {
                return Err(McpError::ConnectionFailed(format!(
                    "server returned {status}"
                )));
            }
            _ => {}
        }

        // Extract Mcp-Session-Id from response headers before consuming the body.
        // WARNING 2: Filter out empty/whitespace-only session IDs.
        let session_id = init_response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // Validate the initialize response is a proper JSON-RPC result.
        let init_body: serde_json::Value = init_response.json().await.map_err(|e| {
            McpError::ProtocolError(format!("failed to parse initialize response: {e}"))
        })?;

        if init_body
            .get("result")
            .and_then(|r| r.get("protocolVersion"))
            .is_none()
        {
            return Err(McpError::ProtocolError(
                "invalid initialize response: missing protocolVersion".to_string(),
            ));
        }

        // Step 2: POST notifications/initialized (with session ID if present).
        let initialized_notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });

        let mut notif_request = client
            .post(url)
            .header("Accept", "application/json, text/event-stream")
            .json(&initialized_notification);
        if let Some(ref sid) = session_id {
            notif_request = notif_request.header("Mcp-Session-Id", sid.as_str());
        }
        let notif_request = apply_connection_headers(notif_request, headers);
        let notif_response = notif_request
            .send()
            .await
            .map_err(|e| McpError::TransportError(e.to_string()))?;

        // WARNING 2: Check HTTP status on the notifications/initialized post —
        // a non-2xx reply means the server rejected the session and further
        // calls are guaranteed to fail.
        let notif_status = notif_response.status().as_u16();
        if !(200..300).contains(&notif_status) {
            return Err(McpError::ProtocolError(format!(
                "server returned HTTP {notif_status} for notifications/initialized"
            )));
        }

        // Step 3: POST tools/list (with session ID if present).
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
            tools_req = tools_req.header("Mcp-Session-Id", sid.as_str());
        }
        let tools_req = apply_connection_headers(tools_req, headers);
        let tools_response = tools_req
            .send()
            .await
            .map_err(|e| McpError::TransportError(e.to_string()))?;

        // WARNING 2: Check tools/list HTTP status before parsing.
        let tools_status = tools_response.status().as_u16();
        if !(200..300).contains(&tools_status) {
            return Err(McpError::ProtocolError(format!(
                "server returned HTTP {tools_status} for tools/list"
            )));
        }

        // Parse the tools/list response.
        let body: serde_json::Value = tools_response
            .json()
            .await
            .map_err(|e| McpError::ProtocolError(e.to_string()))?;

        // Surface JSON-RPC error envelopes instead of producing empty tools.
        if let Some(err) = body.get("error") {
            return Err(jsonrpc_error_from_value(err));
        }

        // WARNING 2: `result.tools` must be a valid JSON array. If it is
        // missing or not an array, surface a ProtocolError rather than
        // silently registering zero tools.
        let tools_val = body
            .get("result")
            .and_then(|r| r.get("tools"))
            .ok_or_else(|| {
                McpError::ProtocolError("tools/list response missing `result.tools`".to_string())
            })?;
        let arr = tools_val.as_array().ok_or_else(|| {
            McpError::ProtocolError("tools/list `result.tools` was not a JSON array".to_string())
        })?;
        let tools = arr
            .iter()
            .filter_map(|v| serde_json::from_value::<McpToolSchema>(v.clone()).ok())
            .collect::<Vec<_>>();

        Ok((tools, session_id))
    }

    /// Connect to an SSE endpoint, discover the POST endpoint from SSE events,
    /// perform the MCP handshake via the discovered endpoint, and keep the
    /// SSE connection alive in a background task.
    ///
    /// Only marks the connection as successful (`handshake_done = true`,
    /// `was_connected = true`) AFTER the handshake actually succeeds. If
    /// endpoint discovery or the HTTP handshake fails, the connection is
    /// left in a non-handshaked state so callers can retry or surface the
    /// failure.
    async fn perform_sse_handshake(&mut self, key: &str) {
        let conn = match self.connections.get(key) {
            Some(c) => c,
            None => return,
        };
        let sse_url = conn.url.clone();
        let parsed = match url::Url::parse(&sse_url) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    server = %key,
                    error = %e,
                    "MCP SSE handshake failure: invalid URL"
                );
                return;
            }
        };

        // WARNING 3: Legacy SSE uses raw TcpStream for keepalive, which
        // cannot carry HTTPS traffic. Reject https:// URLs on legacy SSE
        // rather than silently sending plaintext HTTP through a TLS port.
        if parsed.scheme() == "https" {
            tracing::warn!(
                server = %key,
                url = %sse_url,
                "MCP SSE handshake failure: legacy SSE transport does not support HTTPS — \
                 use streamable HTTP transport (transport = \"http\") instead"
            );
            return;
        }

        let host = parsed.host_str().unwrap_or("127.0.0.1").to_string();
        let port = parsed.port().unwrap_or(80);
        let path = parsed.path().to_string();
        let addr = format!("{host}:{port}");
        let base_url = format!("{}://{}:{}", parsed.scheme(), host, port);

        // Discover the POST endpoint via reqwest SSE request.
        let post_endpoint = {
            let client = reqwest::Client::builder().build().ok();

            let mut discovered_endpoint: Option<String> = None;

            if let Some(client) = client {
                let response = client
                    .get(&sse_url)
                    .header("Accept", "text/event-stream")
                    .send()
                    .await;

                if let Ok(mut response) = response {
                    let mut accumulated = String::new();
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                    while discovered_endpoint.is_none() {
                        let remaining =
                            deadline.saturating_duration_since(tokio::time::Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        match tokio::time::timeout(remaining, response.chunk()).await {
                            Ok(Ok(Some(chunk))) => {
                                accumulated.push_str(&String::from_utf8_lossy(&chunk));
                                discovered_endpoint = parse_sse_endpoint(&accumulated);
                            }
                            _ => break,
                        }
                    }
                }
            }

            discovered_endpoint.map(|ep| {
                if ep.starts_with("http://") || ep.starts_with("https://") {
                    ep
                } else {
                    format!("{base_url}{ep}")
                }
            })
        };

        // If endpoint discovery failed, abort. Do NOT mark the connection
        // as handshake_done — leave it in a retryable state.
        let endpoint = match post_endpoint {
            Some(ep) => ep,
            None => {
                tracing::warn!(
                    server = %key,
                    "MCP SSE handshake failure: endpoint discovery produced no result"
                );
                return;
            }
        };

        // Perform the MCP handshake via the discovered POST endpoint BEFORE
        // opening any keepalive stream. If the handshake fails, there is
        // nothing worth keeping alive.
        let tools = match self.perform_http_handshake(&endpoint).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    server = %key,
                    error = %e,
                    "MCP SSE handshake failure"
                );
                return;
            }
        };

        // Open a raw TCP connection for the background SSE keepalive task.
        // Only reached after handshake success.
        let stream = {
            use tokio::io::AsyncWriteExt;

            match tokio::net::TcpStream::connect(&addr).await {
                Ok(mut s) => {
                    let request = format!(
                        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\nCache-Control: no-cache\r\n\r\n"
                    );
                    if s.write_all(request.as_bytes()).await.is_err() {
                        tracing::warn!(
                            server = %key,
                            "MCP SSE handshake failure: could not write keepalive request"
                        );
                        return;
                    }
                    s
                }
                Err(e) => {
                    tracing::warn!(
                        server = %key,
                        error = %e,
                        "MCP SSE handshake failure: could not open keepalive TCP stream"
                    );
                    return;
                }
            }
        };

        // Keep the SSE connection alive in a background task.
        let sse_handle = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(_n) => {
                        // Event received; keep reading to maintain persistence.
                    }
                    Err(_) => break,
                }
            }
        });

        if let Some(conn) = self.connections.get_mut(key) {
            conn.transport_mode = Some(TransportMode::LegacySse {
                post_endpoint: endpoint,
                sse_handle,
            });
            conn.tools = tools;
            conn.handshake_done = true;
            conn.was_connected = true;
        }
    }

    /// Perform the MCP HTTP handshake: initialize then tools/list.
    ///
    /// Returns discovered tools, or an empty vec if the handshake
    /// fails (non-fatal — the connection is still registered for
    /// lazy retry later).
    async fn perform_http_handshake(&self, url: &str) -> Result<Vec<McpToolSchema>, McpError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| McpError::TransportError(e.to_string()))?;

        // Step 1: Send the "initialize" JSON-RPC request.
        let initialize_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "simulacra",
                    "version": "0.1.0"
                }
            }
        });

        let init_response = client
            .post(url)
            .json(&initialize_request)
            .send()
            .await
            .map_err(|e| McpError::TransportError(e.to_string()))?;

        // WARNING 2: Fail fast on non-2xx initialize responses rather than
        // silently continuing with a broken session.
        let init_status = init_response.status().as_u16();
        if !(200..300).contains(&init_status) {
            return Err(McpError::ProtocolError(format!(
                "server returned HTTP {init_status} for initialize"
            )));
        }

        // Step 1b: Send the "notifications/initialized" JSON-RPC notification.
        let initialized_notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });

        let notif_response = client
            .post(url)
            .json(&initialized_notification)
            .send()
            .await
            .map_err(|e| McpError::TransportError(e.to_string()))?;

        let notif_status = notif_response.status().as_u16();
        if !(200..300).contains(&notif_status) {
            return Err(McpError::ProtocolError(format!(
                "server returned HTTP {notif_status} for notifications/initialized"
            )));
        }

        // Step 2: Send the "tools/list" JSON-RPC request.
        let tools_list_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });

        let tools_response = client
            .post(url)
            .json(&tools_list_request)
            .send()
            .await
            .map_err(|e| McpError::TransportError(e.to_string()))?;

        let tools_status = tools_response.status().as_u16();
        if !(200..300).contains(&tools_status) {
            return Err(McpError::ProtocolError(format!(
                "server returned HTTP {tools_status} for tools/list"
            )));
        }

        // Parse the tools/list response to extract tool schemas.
        let body: serde_json::Value = tools_response
            .json()
            .await
            .map_err(|e| McpError::ProtocolError(e.to_string()))?;

        // Surface JSON-RPC error envelopes instead of producing empty tools.
        if let Some(err) = body.get("error") {
            return Err(jsonrpc_error_from_value(err));
        }

        // WARNING 2: `result.tools` must be a valid JSON array.
        let tools_val = body
            .get("result")
            .and_then(|r| r.get("tools"))
            .ok_or_else(|| {
                McpError::ProtocolError("tools/list response missing `result.tools`".to_string())
            })?;
        let arr = tools_val.as_array().ok_or_else(|| {
            McpError::ProtocolError("tools/list `result.tools` was not a JSON array".to_string())
        })?;
        let tools = arr
            .iter()
            .filter_map(|v| serde_json::from_value::<McpToolSchema>(v.clone()).ok())
            .collect::<Vec<_>>();

        Ok(tools)
    }

    /// Bridge an MCP tool schema to a Simulacra ToolDefinition.
    fn bridge_tool_schema(schema: &McpToolSchema) -> ToolDefinition {
        ToolDefinition {
            name: schema.name.clone(),
            description: schema.description.clone(),
            input_schema: schema.input_schema.clone(),
        }
    }

    /// List tool definitions from all connected MCP servers.
    ///
    /// Triggers the lazy MCP handshake for any servers that have not
    /// yet been initialized, then aggregates all discovered tools.
    pub async fn list_tools(&mut self) -> Vec<ToolDefinition> {
        self.ensure_connected().await;
        self.connections
            .values()
            .flat_map(|conn| conn.tools.iter().map(Self::bridge_tool_schema))
            .collect()
    }

    /// List tool definitions grouped by server name.
    ///
    /// Returns `(server_name, ToolDefinition)` pairs so callers can route
    /// `call_tool` requests back to the correct server.
    pub async fn list_tools_by_server(&mut self) -> Vec<(String, ToolDefinition)> {
        self.ensure_connected().await;
        self.connections
            .iter()
            .flat_map(|(server_name, conn)| {
                conn.tools
                    .iter()
                    .map(|t| (server_name.clone(), Self::bridge_tool_schema(t)))
            })
            .collect()
    }

    /// Register an MCP server with an explicit name (used as the routing key
    /// for `call_tool`), rather than deriving the name from the URL hostname.
    ///
    /// WARNING 4: Only `None`, `Some("")`, `Some("auto")`, `Some("sse")`,
    /// and `Some("http")` are accepted transport values. Any other value is rejected rather
    /// than silently falling through to auto-detect.
    pub async fn connect_named(
        &mut self,
        name: &str,
        url: &str,
        transport: Option<&str>,
    ) -> Result<(), McpError> {
        self.connect_named_with_headers(name, url, transport, Vec::new())
            .await
    }

    /// Register a named MCP server and attach headers to streamable HTTP
    /// handshake requests for that connection.
    pub async fn connect_named_with_headers(
        &mut self,
        name: &str,
        url: &str,
        transport: Option<&str>,
        headers: Vec<(String, String)>,
    ) -> Result<(), McpError> {
        let transport = normalize_transport(transport)?;

        // Validate the URL is parseable.
        let _parsed = url::Url::parse(url).map_err(|e| {
            let error = e.to_string();
            tracing::warn!(server = name, error = %error, "WARN: MCP connection failure");
            McpError::ConnectionFailed(format!("connection to {url} failed: {e}"))
        })?;

        self.connections.insert(
            name.to_string(),
            McpConnection::new(name.to_string(), url.to_string(), transport, headers),
        );

        Ok(())
    }

    /// Register a WASM MCP server by module path.
    ///
    /// Phase 1c keeps this as a *structural* register stub (no compile, no
    /// instantiate) on purpose: the wasm_mcp_config tests that exercise
    /// capability-ordering need the connection to exist so the call_tool
    /// path runs the cap check at all. The dispatch-time WASM behavior is
    /// still `unimplemented!()` via `WasmMcpModule::dispatch` below.
    #[cfg(feature = "wasm")]
    pub async fn connect_wasm_named(
        &mut self,
        name: &str,
        module_id: &str,
    ) -> Result<(), McpError> {
        self.connections.insert(
            name.to_string(),
            McpConnection {
                server_name: name.to_string(),
                url: "http://127.0.0.1/".to_string(),
                headers: Vec::new(),
                tools: Vec::new(),
                handshake_done: false,
                was_connected: false,
                transport_mode: Some(TransportMode::Wasm {
                    module_id: module_id.to_string(),
                }),
                configured_transport: Some("wasm".to_string()),
            },
        );

        Ok(())
    }

    /// Register a WASM MCP server using a fully-loaded [`WasmMcpModule`].
    ///
    /// The module's `(ToolDefinition, WasmTool)` pairs are installed on the
    /// connection so `list_tools` returns them and `call_tool` can dispatch
    /// through the cached component. The connection is marked
    /// `handshake_done = true` because all the work `list-tools` would have
    /// done over the wire has already happened in `load_wasm_mcp_module`.
    pub async fn connect_wasm_module(
        &mut self,
        name: &str,
        module: WasmMcpModule,
    ) -> Result<(), McpError> {
        #[cfg(feature = "wasm")]
        {
            // S041 §Observability: WASM MCP servers complete their handshake
            // in-process (the module's `list-tools` already ran during
            // `load_wasm_mcp_module`), but we still emit a
            // `simulacra_mcp_handshake` span so o11y consumers can correlate
            // WASM and HTTP/SSE MCP transports under a single span name.
            let handshake_span = tracing::info_span!(
                "simulacra_mcp_handshake",
                simulacra.mcp.transport_mode = "wasm",
                simulacra.mcp.module_id = name,
            );
            let _handshake_guard = handshake_span.enter();

            // Bridge each ToolDefinition into the existing McpToolSchema shape
            // so the rest of the manager (list_tools, etc.) treats wasm
            // transports identically to HTTP/SSE.
            let tool_schemas = module
                .tools
                .iter()
                .map(|def| McpToolSchema {
                    name: def.name.clone(),
                    description: def.description.clone(),
                    input_schema: def.input_schema.clone(),
                })
                .collect();

            self.connections.insert(
                name.to_string(),
                McpConnection {
                    server_name: name.to_string(),
                    url: String::new(),
                    headers: Vec::new(),
                    tools: tool_schemas,
                    handshake_done: true,
                    was_connected: true,
                    transport_mode: Some(TransportMode::Wasm {
                        module_id: name.to_string(),
                    }),
                    configured_transport: Some("wasm".to_string()),
                },
            );

            self.wasm_modules.insert(name.to_string(), module);
            Ok(())
        }
        #[cfg(not(feature = "wasm"))]
        {
            let _ = (name, module);
            Err(McpError::ConnectionFailed(
                "WASM MCP support is disabled (simulacra-mcp built without `wasm` feature)"
                    .to_string(),
            ))
        }
    }

    /// Install an `AtomicUsize` that the runtime increments every time it
    /// instantiates a wasmtime component for a WASM MCP call. Used by
    /// `wasm_mcp_transport.rs` to verify the agent-fuel short-circuit
    /// happens before any wasmtime work.
    pub fn set_instantiation_recorder(&mut self, recorder: Arc<AtomicUsize>) {
        self.instantiation_recorder = Some(recorder);
    }

    /// Call a tool on the named MCP server.
    ///
    /// The capability proxy layer checks the `CapabilityToken` before
    /// dispatching the call to ensure `mcp_tools` contains the requested tool.
    ///
    /// Before dispatching, the call appends a Journal entry of kind ToolCall
    /// so the conversation replay log captures every MCP tool invocation
    /// (journal before side effect — the Golden Rule).
    ///
    /// **Agent attribution.** This signature does not carry an agent ID —
    /// it is preserved for callers that don't need per-agent journal
    /// attribution (single-agent CLI processes). For shared-process
    /// deployments where multiple agents share one `McpManager`
    /// (e.g. `simulacra-server`), use [`call_tool_for_agent`] so each
    /// outbound `simulacra:mcp/http.fetch` journal entry carries the calling
    /// agent's ID.
    ///
    /// [`call_tool_for_agent`]: Self::call_tool_for_agent
    pub async fn call_tool(
        &mut self,
        server: &str,
        tool: &str,
        input: serde_json::Value,
        capability: &CapabilityToken,
    ) -> Result<serde_json::Value, McpError> {
        // Empty AgentId means "let the WASM module's bake-in default
        // win, if any" inside the dispatch chain — preserves the
        // existing CLI behavior where `WasmMcpServerDescriptor.agent_id`
        // is the only source.
        self.call_tool_for_agent(&AgentId(String::new()), server, tool, input, capability)
            .await
    }

    /// Like [`call_tool`] but stamps the per-call `agent_id` onto every
    /// downstream journal entry written by the dispatch path (notably
    /// the WASM transport's `simulacra:mcp/http.fetch` audit entries).
    ///
    /// Use this in shared-process deployments (`simulacra-server`) where one
    /// `McpManager` instance is reused across many concurrent agents and
    /// the audit trail needs to attribute each outbound HTTP call to the
    /// agent that made it. A non-empty `agent_id` always overrides any
    /// `WasmMcpModule::with_agent_id` default; an empty `agent_id` falls
    /// back to the module's bake-in (preserving CLI semantics).
    ///
    /// [`call_tool`]: Self::call_tool
    pub async fn call_tool_for_agent(
        &mut self,
        agent_id: &AgentId,
        server: &str,
        tool: &str,
        input: serde_json::Value,
        capability: &CapabilityToken,
    ) -> Result<serde_json::Value, McpError> {
        self.check_capability(server, tool, capability)?;

        // Ensure the server has completed its MCP handshake before dispatching.
        self.ensure_server_connected(server).await;
        if !self.connection_handshake_done(server) {
            return Err(Self::handshake_failed_error(server));
        }

        let source = format!("mcp:{server}");

        let span = tracing::info_span!(
            "execute_tool",
            gen_ai.operation.name = "execute_tool",
            simulacra.tool.name = tool,
            simulacra.tool.source = %source,
        );

        // Log inside a synchronous span guard that is dropped before awaits.
        {
            let _guard = span.enter();

            tracing::info!(
                counter.simulacra.mcp.calls = 1,
                server = server,
                tool = tool,
                "MCP tool call"
            );

            tracing::info!(
                event = "gen_ai.tool.message",
                simulacra.tool.source = %source,
                input = %input,
                "MCP tool input"
            );
        }

        // Journal before side effect (Golden Rule).
        // If the journal append fails, abort — DO NOT execute the side effect.
        self.append_journal_tool_call(tool, &input)?;

        let call_start = std::time::Instant::now();
        let result = self
            .dispatch_with_reconnect(agent_id, server, tool, &input)
            .await;

        // S010: Record OTel meter observations for MCP tool call
        let meters = McpMeters::get();
        let attrs = &[
            KeyValue::new("server", server.to_owned()),
            KeyValue::new("tool", tool.to_owned()),
        ];
        meters
            .tool_duration
            .record(call_start.elapsed().as_secs_f64() * 1000.0, attrs);
        meters.calls.add(1, attrs);
        if result.is_err() {
            meters.tool_errors.add(1, attrs);
        }

        let output = result?;

        {
            let _guard = span.enter();
            // Do NOT log full output content — it may contain sensitive data
            // returned from the MCP server (secrets, tokens, PII). Emit the
            // length only, matching the `gen_ai.tool.result_length` pattern
            // used by simulacra-tool.
            let output_length = output.to_string().len();
            tracing::info!(
                event = "gen_ai.tool.message",
                simulacra.tool.source = %source,
                gen_ai.tool.result_length = output_length,
                "MCP tool output"
            );
        }

        Ok(output)
    }

    /// Dispatch a tool call, retrying with exponential backoff if the server
    /// was previously connected but the transport now fails.
    ///
    /// Backoff schedule: 1s, 2s, 4s (3 retry attempts max).
    /// On each retry the connection handshake is re-performed so that
    /// transient network failures or server restarts are recovered automatically.
    ///
    /// S024 Session expiry: If the error is "session expired" (HTTP 404 with
    /// active session), the session ID and handshake state are cleared, a fresh
    /// handshake is performed immediately (no backoff), and the original
    /// request is retried once.
    async fn dispatch_with_reconnect(
        &mut self,
        agent_id: &AgentId,
        server: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        const MAX_RETRIES: u32 = 3;
        let base_backoff_ms = self.reconnect_base_delay_ms;

        let first_err = match self.dispatch_tool_call(agent_id, server, tool, input).await {
            Ok(output) => return Ok(output),
            Err(e) => e,
        };

        // S024: Session expiry — immediate re-handshake, no backoff, retry once.
        // Session IDs only exist in streamable HTTP mode, so always reconnect
        // as streamable HTTP (force configured_transport = "http").
        if matches!(&first_err, McpError::ProtocolError(msg) if msg.contains("session expired")) {
            tracing::info!(
                server = %server,
                "MCP session expired, re-initializing"
            );
            McpMeters::get()
                .session_expired
                .add(1, &[KeyValue::new("server", server.to_owned())]);
            // Clear handshake state and force streamable HTTP for reconnect.
            if let Some(conn) = self.connections.get_mut(server) {
                conn.handshake_done = false;
                conn.transport_mode = None;
                conn.configured_transport = Some("http".to_string());
            }
            self.ensure_server_connected(server).await;
            if !self.connection_handshake_done(server) {
                return Err(Self::handshake_failed_error(server));
            }
            // Retry once after successful re-handshake.
            return self.dispatch_tool_call(agent_id, server, tool, input).await;
        }

        // Non-transport errors are not retriable.
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

            // Reset handshake state so ensure_server_connected will re-handshake.
            // Pin configured_transport to the previously detected mode so we don't
            // re-auto-detect and potentially switch transports on reconnect.
            if let Some(conn) = self.connections.get_mut(server) {
                if conn.configured_transport.is_none() {
                    // Lock in the detected transport for reconnection.
                    match &conn.transport_mode {
                        Some(TransportMode::StreamableHttp { .. }) => {
                            conn.configured_transport = Some("http".to_string());
                        }
                        Some(TransportMode::LegacySse { .. }) => {
                            conn.configured_transport = Some("sse".to_string());
                        }
                        #[cfg(feature = "wasm")]
                        Some(TransportMode::Wasm { .. }) => {}
                        _ => {}
                    }
                }
                conn.handshake_done = false;
                conn.transport_mode = None;
            }

            self.ensure_server_connected(server).await;
            if !self.connection_handshake_done(server) {
                let err = Self::handshake_failed_error(server);
                tracing::warn!(
                    server = %server,
                    attempt = attempt + 1,
                    error = %err,
                    "MCP reconnection handshake failed"
                );
                last_err = err;
                continue;
            }

            match self.dispatch_tool_call(agent_id, server, tool, input).await {
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

    fn connection_handshake_done(&self, server: &str) -> bool {
        self.connections
            .get(server)
            .map(|c| c.handshake_done)
            .unwrap_or(false)
    }

    fn handshake_failed_error(server: &str) -> McpError {
        McpError::ConnectionFailed(format!("MCP handshake failed for server {server}"))
    }

    /// Check whether an error is a transport-level failure that could be
    /// recovered by reconnecting.
    fn is_transport_error(&self, err: &McpError) -> bool {
        matches!(
            err,
            McpError::TransportError(_) | McpError::ConnectionFailed(_)
        )
    }

    /// Append a Journal ToolCall entry if a journal storage backend is configured.
    ///
    /// Returns an error if the journal append fails. The caller MUST NOT proceed
    /// with the side effect (MCP dispatch) when this returns Err — a missing
    /// journal entry makes replay non-deterministic. See the "Journal Before
    /// Return" invariant in ARCHITECTURE.md.
    fn append_journal_tool_call(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(), McpError> {
        if let Some(ref journal) = self.journal {
            let entry = JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: self.agent_id.clone(),
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                entry: JournalEntryKind::ToolCall {
                    tool_call_id: None,
                    tool_name: tool_name.to_string(),
                    arguments: arguments.clone(),
                },
            };
            journal.append(entry).map_err(|e| {
                tracing::warn!(
                    error = %e,
                    tool = tool_name,
                    "journal append failed — aborting MCP dispatch to preserve replay determinism"
                );
                McpError::ProtocolError(format!("journal append failed: {e}"))
            })?;
        }
        Ok(())
    }

    /// Verify the capability token grants access to the requested MCP tool.
    ///
    /// MCP tool capabilities use the fully-qualified `mcp:{server}:{tool}`
    /// namespace so that grants are scoped to a specific server. Bare tool
    /// names in `mcp_tools` do NOT authorize tools across every server —
    /// every pattern MUST be in the `mcp:{server}:{tool}` form (with glob
    /// wildcards, e.g. `mcp:github:*` or `mcp:*:*`).
    ///
    /// Patterns that do not start with `mcp:` are ignored for MCP dispatch
    /// and treated as non-matches.
    fn check_capability(
        &self,
        server: &str,
        tool: &str,
        capability: &CapabilityToken,
    ) -> Result<(), McpError> {
        let qualified = format!("mcp:{server}:{tool}");
        if !capability
            .mcp_tools
            .iter()
            .any(|pattern| pattern.starts_with("mcp:") && glob_match(pattern, &qualified))
        {
            return Err(McpError::CapabilityDenied(format!(
                "tool {tool} on server {server} not in granted mcp_tools \
                 (patterns must be in the form mcp:{{server}}:{{tool}})"
            )));
        }
        Ok(())
    }

    /// Dispatch the actual JSON-RPC `tools/call` request to the MCP server.
    ///
    /// Dispatch a tool call to a wasm-transport MCP server.
    ///
    /// Order of operations matches the spec § Tool dispatch:
    ///   1. Look up the loaded module (must exist post-handshake).
    ///   2. Resolve the tool by name on the module's discovered tool list;
    ///      unknown tools surface as `ProtocolError("execution failed: ...")`
    ///      so the agent-facing `ToolError::ExecutionFailed` round-trip works.
    ///   3. Pre-flight the agent fuel budget: `Some(0)` short-circuits
    ///      WITHOUT instantiating the component. The instantiation
    ///      recorder (if installed) ticks only AFTER this check passes.
    ///   4. On a blocking pool, build a fresh `Linker` + `Store`, instantiate
    ///      the module's compiled `simulacra:mcp/server` Component, and call
    ///      `call-tool`. The store carries the per-call WASI ctx, the
    ///      module's allowlist/hooks/journal, and the captured runtime
    ///      handle so the `simulacra:mcp/http.fetch` host import bridges sync
    ///      → async cleanly.
    ///   5. Read the post-call fuel residual via `store.get_fuel()` so
    ///      the span carries `simulacra.wasm.fuel_consumed`. Map
    ///      `tool-error::invalid-arguments`/`execution-failed` to
    ///      `McpError::ProtocolError` so the existing reconnect/retry
    ///      plumbing treats them uniformly.
    #[cfg(feature = "wasm")]
    async fn dispatch_wasm_tool_call(
        &self,
        agent_id: &AgentId,
        server: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        // S041 §Observability: dedicated `simulacra_mcp_tool_call` span carries
        // `simulacra.wasm.fuel_consumed` so the WASM transport's fuel
        // accounting surfaces alongside the standard MCP span hierarchy.
        let tool_call_span = tracing::info_span!(
            "simulacra_mcp_tool_call",
            simulacra.mcp.transport_mode = "wasm",
            simulacra.mcp.module_id = server,
            simulacra.tool.name = tool,
            simulacra.wasm.fuel_consumed = tracing::field::Empty,
        );
        let _tool_call_guard = tool_call_span.enter();

        let module = self.wasm_modules.get(server).ok_or_else(|| {
            McpError::ConnectionFailed(format!("no wasm module loaded for server {server}"))
        })?;

        if !module.tools.iter().any(|def| def.name == tool) {
            let msg =
                format!("execution failed: tool '{tool}' not found on wasm MCP server '{server}'");
            tracing::error!(
                server = server,
                tool = tool,
                error = %msg,
                "WASM trap during call_tool: tool not found"
            );
            return Err(McpError::ProtocolError(msg));
        }

        // Agent fuel pre-check: a budget seeded at 0 means **exhausted**
        // (matching the historical `simulacra_wasm::WasmTool` semantics).
        // Short-circuit BEFORE any wasmtime work so the instantiation
        // recorder stays at zero.
        if let Some(ref counter) = self.agent_fuel_remaining
            && counter.load(Ordering::SeqCst) == 0
        {
            return Err(McpError::ProtocolError(
                "execution failed: agent fuel budget exhausted".to_string(),
            ));
        }

        if let Some(ref recorder) = self.instantiation_recorder {
            recorder.fetch_add(1, Ordering::SeqCst);
        }

        let engine = module.engine.clone();
        let component = module.component.clone();
        let allowlist = module.allowlist.clone();
        let hooks = module.hooks.clone();
        let journal = module.journal.clone();
        // Per-call agent_id wins when non-empty so a shared `McpManager`
        // (e.g. simulacra-server) can attribute each fetch to the agent
        // that triggered it. An empty per-call agent_id falls back to
        // the module's bake-in default (CLI back-compat).
        let agent_id = if agent_id.0.is_empty() {
            module.agent_id.clone()
        } else {
            agent_id.clone()
        };
        let http_client = module.http_client.clone();
        // Per-call fuel ceiling = min(server_fuel, agent_remaining_fuel)
        // per spec § Tool dispatch. `0` on either side means "unlimited
        // from that side", so the cap collapses to whichever is finite.
        // Both unlimited stays unlimited (fuel_limit = 0 → store gets
        // u64::MAX in `build_wasm_mcp_store`).
        let server_fuel = module.fuel_limit;
        let agent_remaining = self
            .agent_fuel_remaining
            .as_ref()
            .map(|c| c.load(Ordering::SeqCst));
        let fuel_limit = match (server_fuel, agent_remaining) {
            (0, None) => 0,
            (0, Some(0)) => unreachable!("agent fuel == 0 short-circuited above"),
            (0, Some(remaining)) => remaining,
            (server, None) => server,
            (server, Some(0)) => {
                let _ = server;
                unreachable!("agent fuel == 0 short-circuited above")
            }
            (server, Some(remaining)) => server.min(remaining),
        };
        let server_name = server.to_string();
        let tool_name = tool.to_string();
        let args_json = serde_json::to_string(input).map_err(|e| {
            McpError::ProtocolError(format!("execution failed: invalid arguments: {e}"))
        })?;

        // Capture the current runtime handle so the sync host import
        // (`simulacra:mcp/http.fetch`) can bridge into async fetch from
        // inside spawn_blocking.
        let runtime_handle = tokio::runtime::Handle::current();

        // The blocking closure returns the raw call outcome paired with
        // the post-call fuel consumption. The outcome carries the
        // module's own `tool-error` payload (`Ok` | `Err(ToolError)`)
        // OR a wasmtime trap that we pre-classify as `Err(McpError)`
        // here so the caller sees recognizable messages (notably
        // "fuel exhausted" for `Trap::OutOfFuel`).
        let blocking_handle = runtime_handle.clone();
        type ToolCallResult =
            Result<Result<String, wit_server::simulacra::mcp::types::ToolError>, McpError>;
        let blocking_result =
            tokio::task::spawn_blocking(move || -> Result<(ToolCallResult, u64), McpError> {
                let mut store = build_wasm_mcp_store(
                    &engine,
                    fuel_limit,
                    &server_name,
                    allowlist,
                    hooks,
                    journal,
                    agent_id,
                    http_client,
                    blocking_handle,
                )?;
                let linker = build_wasm_mcp_linker(&engine)?;
                let server = wit_server::Server::instantiate(&mut store, &component, &linker)
                    .map_err(|e| {
                        McpError::ConnectionFailed(format!("wasm instantiation failed: {e}"))
                    })?;
                let call_result = server.call_call_tool(&mut store, &tool_name, &args_json);
                // On `get_fuel` error (engine misconfigured / fuel
                // disabled), fall back to "consumed = 0" rather than
                // "consumed = fuel_limit" so we don't double-charge an
                // agent for a runtime bug. `unwrap_or(0)` here means
                // "unknown consumption → don't deduct"; the engine
                // bug surfaces elsewhere.
                //
                // When `fuel_limit == 0` (unlimited per spec), the store
                // was seeded with `u64::MAX` in `build_wasm_mcp_store` —
                // residual subtraction still yields actual consumption,
                // so the agent's `ResourceBudget` (when present) and the
                // `simulacra.wasm.fuel_consumed` span field reflect real
                // usage even for uncapped modules.
                let fuel_remaining = store.get_fuel().unwrap_or(0);
                let initial_fuel = if fuel_limit == 0 {
                    u64::MAX
                } else {
                    fuel_limit
                };
                let consumed = initial_fuel.saturating_sub(fuel_remaining);
                let outcome: ToolCallResult = match call_result {
                    Ok(inner) => Ok(inner),
                    Err(e) => {
                        // Out-of-fuel traps must surface a recognizable
                        // "fuel exhausted" message so reconnect/retry
                        // plumbing can distinguish budget exhaustion
                        // from other execution failures.
                        let msg = if e
                            .downcast_ref::<wasmtime::Trap>()
                            .is_some_and(|t| matches!(t, wasmtime::Trap::OutOfFuel))
                        {
                            "execution failed: fuel exhausted".to_string()
                        } else {
                            format!("execution failed: {e}")
                        };
                        Err(McpError::ProtocolError(msg))
                    }
                };
                Ok((outcome, consumed))
            })
            .await
            .map_err(|e| McpError::ProtocolError(format!("execution failed: {e}")));

        let (call_result, consumed) = blocking_result??;

        // Decrement the agent-level fuel budget by whatever this call
        // consumed (success and trap paths alike) so a long-running
        // agent monotonically draws down its budget.
        if let Some(ref counter) = self.agent_fuel_remaining {
            let prev = counter.load(Ordering::SeqCst);
            let next = prev.saturating_sub(consumed);
            counter.store(next, Ordering::SeqCst);
        }

        // S041 §Observability: surface the per-call fuel accounting on
        // the span. `simulacra.wasm.fuel_consumed` is computed from the
        // store's residual fuel (initial budget minus what's left after
        // the call returns).
        tool_call_span.record("simulacra.wasm.fuel_consumed", consumed);

        // S041 §Observability: record `simulacra.wasm.fuel_consumed` via
        // the OTel meter directly (the tracing-field histogram
        // convention isn't picked up by every downstream bridge —
        // notably the local Aniani instance — so we use the
        // explicit `Histogram::record` path that already works for
        // `simulacra.mcp.tool.duration`).
        McpMeters::get().wasm_fuel_consumed.record(
            consumed,
            &[
                KeyValue::new("module", server.to_owned()),
                KeyValue::new("tool", tool.to_owned()),
            ],
        );
        // Mirror the metric as a structured log so log-only consumers
        // can still see per-call fuel consumption.
        tracing::info!(
            module = server,
            tool = tool,
            value = consumed,
            "WASM MCP fuel consumed"
        );

        match call_result {
            Ok(Ok(s)) => serde_json::from_str(&s).map_err(|e| {
                McpError::ProtocolError(format!(
                    "execution failed: tool returned non-JSON output: {e}"
                ))
            }),
            Ok(Err(wit_server::simulacra::mcp::types::ToolError::InvalidArguments(msg))) => Err(
                McpError::ProtocolError(format!("execution failed: invalid arguments: {msg}")),
            ),
            Ok(Err(wit_server::simulacra::mcp::types::ToolError::ExecutionFailed(msg))) => {
                // Module-reported execution failure (e.g. invalid input,
                // upstream error). NOT a wasmtime trap — the module ran
                // to completion and signalled failure via its own error
                // channel. Spec § Observability reserves `tracing::error!`
                // for WASM traps; module-level failures are a `warn!`.
                tracing::warn!(
                    server = server,
                    tool = tool,
                    error = %msg,
                    "WASM MCP tool reported execution failure"
                );
                Err(McpError::ProtocolError(format!("execution failed: {msg}")))
            }
            Err(err) => {
                // Real wasmtime trap (out-of-fuel, divide-by-zero, host
                // import bridge failure, etc.) — keep `tracing::error!`
                // per spec line 414.
                tracing::error!(
                    server = server,
                    tool = tool,
                    error = %err,
                    "WASM trap during call_tool"
                );
                Err(err)
            }
        }
    }

    /// For StreamableHttp mode:
    /// - Sends `Accept: application/json, text/event-stream` and `Mcp-Session-Id`.
    /// - Checks for HTTP 404 with a stored session ID → returns a session-expired
    ///   `ProtocolError` so `dispatch_with_reconnect` can handle it.
    /// - Branches on response `Content-Type`: JSON is parsed directly, SSE is
    ///   streamed via `parse_sse_tool_response`.
    async fn dispatch_tool_call(
        &self,
        agent_id: &AgentId,
        server: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let conn = self.connections.get(server).ok_or_else(|| {
            McpError::ConnectionFailed(format!("no connection to server {server}"))
        })?;

        let _tool_exists = conn.tools.iter().any(|t| t.name == tool);

        // Wasm transport: dispatch in-process. Returns directly — no HTTP
        // path is taken.
        #[cfg(feature = "wasm")]
        if matches!(&conn.transport_mode, Some(TransportMode::Wasm { .. })) {
            return self
                .dispatch_wasm_tool_call(agent_id, server, tool, input)
                .await;
        }

        // Determine the target URL and session ID based on transport mode.
        let (target_url, session_id) = match &conn.transport_mode {
            Some(TransportMode::LegacySse { post_endpoint, .. }) => (post_endpoint.clone(), None),
            Some(TransportMode::StreamableHttp { session_id }) => {
                (conn.url.clone(), session_id.clone())
            }
            #[cfg(feature = "wasm")]
            Some(TransportMode::Wasm { .. }) => {
                // Already handled above; unreachable in practice.
                return Err(McpError::ProtocolError(
                    "wasm transport reached HTTP dispatch path".to_string(),
                ));
            }
            Some(TransportMode::LegacyHttp) => (conn.url.clone(), None),
            None => return Err(Self::handshake_failed_error(server)),
        };

        let headers = conn.headers.clone();
        let is_streamable = matches!(
            conn.transport_mode,
            Some(TransportMode::StreamableHttp { .. })
        );

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

        let mut request = client.post(&target_url).json(&call_request);

        // Streamable HTTP: include Accept and session ID headers.
        if is_streamable {
            request = request.header("Accept", "application/json, text/event-stream");
            if let Some(ref sid) = session_id {
                request = request.header("Mcp-Session-Id", sid.as_str());
            }
        }

        request = apply_connection_headers(request, &headers);

        let response = request
            .send()
            .await
            .map_err(|e| McpError::TransportError(e.to_string()))?;

        // S024: HTTP 404 with a stored session ID → session expired.
        let status = response.status().as_u16();
        if is_streamable && status == 404 && session_id.is_some() {
            return Err(McpError::ProtocolError(
                "session expired: server returned 404 with active session".to_string(),
            ));
        }

        // General HTTP error check — don't fall through to JSON parsing on error.
        if status >= 400 {
            return Err(McpError::TransportError(format!(
                "server returned HTTP {status}"
            )));
        }

        // S024: Branch on Content-Type for streamable HTTP responses.
        if is_streamable {
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            // Record response type on the current span.
            if content_type.contains("text/event-stream") {
                tracing::Span::current().record("simulacra.mcp.response_type", "sse_stream");
                return self.parse_sse_tool_response(response, server).await;
            } else {
                tracing::Span::current().record("simulacra.mcp.response_type", "json");
            }
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| McpError::ProtocolError(e.to_string()))?;

        // WARNING 1: JSON-RPC error envelopes must NOT be returned as Ok.
        // If the response contains an `error` object, surface it as an error.
        if let Some(err) = body.get("error") {
            return Err(jsonrpc_error_from_value(err));
        }

        // Extract the result from the JSON-RPC response.
        // If there is no `result` key, the response is malformed — the
        // JSON-RPC spec requires exactly one of `result` or `error` on
        // responses — so treat the absence as a protocol error rather than
        // silently returning the raw envelope.
        let result = body.get("result").cloned().ok_or_else(|| {
            McpError::ProtocolError(
                "JSON-RPC response had neither `result` nor `error`".to_string(),
            )
        })?;
        Ok(result)
    }

    /// Parse an SSE streaming response from a tool call.
    ///
    /// Buffers SSE events, logs progress notifications via `tracing::debug!`,
    /// and extracts the final JSON-RPC result. Returns `ProtocolError` if the
    /// stream closes without delivering a result, or `TransportError` on 60s
    /// idle timeout.
    async fn parse_sse_tool_response(
        &self,
        mut response: reqwest::Response,
        server: &str,
    ) -> Result<serde_json::Value, McpError> {
        let mut accumulated = String::new();
        let idle_timeout = std::time::Duration::from_secs(60);
        let mut deadline = tokio::time::Instant::now() + idle_timeout;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(McpError::TransportError(
                    "SSE stream idle timeout (60s)".to_string(),
                ));
            }

            match tokio::time::timeout(remaining, response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    // Reset idle timeout on each received chunk.
                    deadline = tokio::time::Instant::now() + idle_timeout;
                    accumulated.push_str(&String::from_utf8_lossy(&chunk));
                    // Try to parse complete SSE events from the accumulated buffer.
                    while let Some((_event_type, event_data, rest)) =
                        parse_next_sse_event(&accumulated)
                    {
                        accumulated = rest;
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&event_data) {
                            // WARNING 1: JSON-RPC error envelopes must surface
                            // as Err, not be silently returned as Ok.
                            if let Some(err) = json.get("error") {
                                return Err(jsonrpc_error_from_value(err));
                            }
                            if let Some(result) = json.get("result") {
                                return Ok(result.clone());
                            } else if json.get("method").is_some() {
                                tracing::debug!(
                                    server = %server,
                                    method = json["method"].as_str().unwrap_or("unknown"),
                                    "MCP SSE progress notification"
                                );
                            }
                        }
                    }
                }
                Ok(Ok(None)) => {
                    // Stream closed — check if there's a final event in the buffer.
                    return Err(McpError::ProtocolError(
                        "SSE stream closed without delivering a JSON-RPC response".to_string(),
                    ));
                }
                Ok(Err(e)) => return Err(McpError::TransportError(e.to_string())),
                Err(_) => {
                    return Err(McpError::TransportError(
                        "SSE stream idle timeout (60s)".to_string(),
                    ));
                }
            }
        }
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// McpTool — wraps a single MCP server tool as a Simulacra Tool
// ---------------------------------------------------------------------------

use simulacra_types::ToolError;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::Mutex;

/// A wrapper that presents a single MCP server tool as a Simulacra `Tool`.
///
/// Each `McpTool` holds a shared reference to the `McpManager` (behind
/// `Arc<Mutex<..>>`) and the server name + tool definition needed to
/// route `call_tool` requests to the correct MCP server.
pub struct McpTool {
    manager: Arc<Mutex<McpManager>>,
    server_name: String,
    tool_def: ToolDefinition,
}

impl McpTool {
    pub fn new(
        manager: Arc<Mutex<McpManager>>,
        server_name: String,
        tool_def: ToolDefinition,
    ) -> Self {
        Self {
            manager,
            server_name,
            tool_def,
        }
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_def.name
    }
}

impl simulacra_types::Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        self.tool_def.clone()
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let server = self.server_name.clone();
        let tool_name = self.tool_def.name.clone();
        let cap = capability.clone();
        Box::pin(async move {
            let mut manager = self.manager.lock().await;
            manager
                .call_tool(&server, &tool_name, arguments, &cap)
                .await
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))
        })
    }
}

// ---------------------------------------------------------------------------
// create_mcp_tools — integration point for CLI bootstrap
// ---------------------------------------------------------------------------

/// Connect to MCP servers and return `Tool` wrappers for all discovered tools.
///
/// This is the main integration point for the CLI bootstrap. It takes server
/// descriptors as `(name, url, transport)` tuples to avoid a dependency on
/// `simulacra-config`.
///
/// Each server is connected using its config `name` as the routing key (not
/// the URL hostname), so `call_tool` dispatches correctly even when multiple
/// servers share a hostname.
///
/// Servers that fail to connect are logged as warnings and skipped — they do
/// not prevent other servers from registering their tools.
pub async fn create_mcp_tools(
    servers: &[(String, Option<String>, Option<String>)],
) -> Vec<McpTool> {
    create_mcp_tools_with_wasm(servers, &[]).await
}

/// MCP server descriptor for the WASM transport. Carries the per-server
/// `host:port` allowlist that `simulacra:mcp/http.fetch` consults before any
/// outbound HTTP, plus the hook pipeline and journal that govern the
/// fetch path in production. Field shapes mirror `simulacra_config::McpServerConfig`
/// but are repeated here so this crate stays free of a `simulacra-config`
/// dependency.
///
/// Available regardless of the `wasm` feature so consumers (e.g.
/// `simulacra-cli`) can build their bootstrap without re-gating on the
/// feature flag — when `wasm` is disabled, `create_mcp_tools_with_wasm`
/// logs a warning and skips each WASM descriptor.
#[derive(Clone)]
pub struct WasmMcpServerDescriptor {
    pub name: String,
    pub module_path: std::path::PathBuf,
    pub network_allowlist: Vec<String>,
    /// Governance hook pipeline. When set, every `simulacra:mcp/http.fetch`
    /// invocation runs the `Operation::HttpRequest` chain at
    /// `Phase::Before` (before wire dispatch) and `Phase::After`
    /// (before returning to the module).
    pub hooks: Option<Arc<simulacra_hooks::HookPipeline>>,
    /// Journal storage. When set, every fetch attempt writes one
    /// `JournalEntryKind::HttpRequest` entry BEFORE wire dispatch
    /// (Golden Rule).
    pub journal: Option<Arc<dyn JournalStorage>>,
    /// Agent ID used when journaling fetches from this server. Empty
    /// AgentId means "unattributed" (acceptable for shared CLI
    /// bootstrap; agent-scoped journaling is a future spec).
    pub agent_id: AgentId,
}

impl std::fmt::Debug for WasmMcpServerDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmMcpServerDescriptor")
            .field("name", &self.name)
            .field("module_path", &self.module_path)
            .field("network_allowlist", &self.network_allowlist)
            .field("hooks", &self.hooks.is_some())
            .field("journal", &self.journal.is_some())
            .field("agent_id", &self.agent_id)
            .finish()
    }
}

/// Like `create_mcp_tools` but additionally compiles + connects WASM MCP
/// servers (`transport = "wasm"`). All servers — HTTP/SSE plus WASM —
/// share the same `McpManager`, so capability enforcement, tool routing,
/// and observability stay uniform across transports.
///
/// Always callable. When the `wasm` feature is disabled, WASM descriptors
/// are logged at WARN and skipped so the call still returns the network
/// servers' tools.
pub async fn create_mcp_tools_with_wasm(
    network_servers: &[(String, Option<String>, Option<String>)],
    wasm_servers: &[WasmMcpServerDescriptor],
) -> Vec<McpTool> {
    let manager = Arc::new(Mutex::new(McpManager::new()));

    // Connect HTTP / SSE servers first.
    for (name, url, transport) in network_servers {
        let url = match url {
            Some(u) => u.as_str(),
            None => continue,
        };
        let transport = transport.as_deref();
        let mut mgr = manager.lock().await;
        if let Err(e) = mgr.connect_named(name, url, transport).await {
            tracing::warn!(
                server = %name,
                error = %e,
                "failed to connect MCP server"
            );
        }
    }

    // Compile + connect WASM modules. Failure to load any one module is
    // logged and the server is skipped — the rest of the registry still
    // boots, mirroring the network-server fallthrough behavior.
    //
    // Each descriptor's hooks + journal flow into the WasmMcpModule so
    // `simulacra:mcp/http.fetch` runs through the same governance pipeline
    // and journal that govern host-side fetches. The seam is fully
    // wired in production, not just in tests.
    #[cfg(feature = "wasm")]
    for descriptor in wasm_servers {
        match load_wasm_mcp_module(&descriptor.module_path) {
            Ok(mut module) => {
                module = module.with_network_allowlist(descriptor.network_allowlist.clone());
                module = module.with_agent_id(descriptor.agent_id.clone());
                if let Some(ref hooks) = descriptor.hooks {
                    module = module.with_hooks(Arc::clone(hooks));
                }
                if let Some(ref journal) = descriptor.journal {
                    module = module.with_journal(Arc::clone(journal));
                }
                let mut mgr = manager.lock().await;
                if let Err(e) = mgr.connect_wasm_module(&descriptor.name, module).await {
                    tracing::warn!(
                        server = %descriptor.name,
                        error = %e,
                        "failed to connect WASM MCP server"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    server = %descriptor.name,
                    module = %descriptor.module_path.display(),
                    error = %e,
                    "failed to load WASM MCP module"
                );
            }
        }
    }
    #[cfg(not(feature = "wasm"))]
    for descriptor in wasm_servers {
        tracing::warn!(
            server = %descriptor.name,
            "WASM MCP server skipped — simulacra-mcp built without `wasm` feature"
        );
    }

    // Trigger handshakes and collect tools with server attribution.
    let tools_by_server = {
        let mut mgr = manager.lock().await;
        mgr.list_tools_by_server().await
    };

    tools_by_server
        .into_iter()
        .map(|(server_name, tool_def)| McpTool::new(Arc::clone(&manager), server_name, tool_def))
        .collect()
}

/// A loaded WASM MCP server module.
///
/// Owns the compiled `wasmtime::component::Component` and the discovered
/// tool definitions. Per-call instantiation is cheap: a fresh `Store` and
/// `Linker` (with `wasi:cli` + `simulacra:mcp/http` host imports) per call.
///
/// Builder-style configuration on the module:
///   * `with_network_allowlist` — populates the `host:port` allowlist that
///     `simulacra:mcp/http.fetch` consults before any wire dispatch.
///   * `with_hooks` — registers a `simulacra_hooks::HookPipeline` that fetches
///     route through (`Operation::HttpRequest`, `Phase::Before`/`After`).
///   * `with_journal` — wires the journal that captures every fetch attempt.
///   * `with_fuel_limit` — overrides the per-call fuel ceiling.
///   * `with_http_client` — overrides the shared `reqwest::Client` so
///     enterprise proxy/CA/mTLS configuration can be threaded in from a
///     central location (e.g. a future `simulacra-http` async client).
#[cfg(feature = "wasm")]
pub struct WasmMcpModule {
    engine: wasmtime::Engine,
    component: wasmtime::component::Component,
    tools: Vec<simulacra_types::ToolDefinition>,
    allowlist: Vec<String>,
    hooks: Option<Arc<simulacra_hooks::HookPipeline>>,
    journal: Option<Arc<dyn JournalStorage>>,
    agent_id: AgentId,
    fuel_limit: u64,
    /// Shared `reqwest::Client` for `simulacra:mcp/http.fetch`. Built once
    /// at module load and cloned into each `WasmMcpServerState` so all
    /// outbound calls share connection-pool / proxy / TLS configuration.
    /// Cloning a `reqwest::Client` is cheap (it's `Arc<ClientInner>`
    /// internally).
    http_client: reqwest::Client,
}

#[cfg(feature = "wasm")]
impl WasmMcpModule {
    /// Replace the per-server `host:port` allowlist consulted by
    /// `simulacra:mcp/http.fetch`.
    pub fn with_network_allowlist(mut self, allowlist: Vec<String>) -> Self {
        self.allowlist = allowlist;
        self
    }

    /// Install a `simulacra_hooks::HookPipeline` that fetches from this module
    /// route through. The pipeline's `Operation::HttpRequest` chain is
    /// invoked at `Phase::Before` and `Phase::After` per fetch.
    pub fn with_hooks(mut self, hooks: Arc<simulacra_hooks::HookPipeline>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Install a journal so every fetch from this module is captured at
    /// the start of the request (Golden Rule).
    pub fn with_journal(mut self, journal: Arc<dyn JournalStorage>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Set the agent ID used when journaling fetches from this module.
    /// Empty AgentId is acceptable for shared-bootstrap deployments;
    /// per-agent attribution is a future spec.
    pub fn with_agent_id(mut self, agent_id: AgentId) -> Self {
        self.agent_id = agent_id;
        self
    }

    /// Override the per-call fuel ceiling. `0` = unlimited.
    pub fn with_fuel_limit(mut self, fuel_limit: u64) -> Self {
        self.fuel_limit = fuel_limit;
        self
    }

    /// Replace the shared `reqwest::Client` used by `simulacra:mcp/http.fetch`.
    ///
    /// Default is a HTTP/1.1-only client without connection pooling — the
    /// shape that the recording-fixture tests rely on. Production
    /// deployments that want HTTP/2, connection reuse, custom proxies, or
    /// custom CA bundles should pass a pre-configured client here.
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = client;
        self
    }
}

#[cfg(feature = "wasm")]
impl std::fmt::Debug for WasmMcpModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmMcpModule")
            .field(
                "tools",
                &self.tools.iter().map(|d| &d.name).collect::<Vec<_>>(),
            )
            .field("allowlist_entries", &self.allowlist.len())
            .field("hooks", &self.hooks.is_some())
            .field("journal", &self.journal.is_some())
            .field("agent_id", &self.agent_id)
            .field("fuel_limit", &self.fuel_limit)
            .finish()
    }
}

#[cfg(not(feature = "wasm"))]
#[derive(Debug, Clone, Default)]
pub struct WasmMcpModule;

/// Default per-call fuel ceiling for WASM MCP tools when no per-server
/// limit is configured. High enough that ordinary tools (echo, counter,
/// reverse) finish well below it; tight infinite loops (the `burn_fuel`
/// fixture) trap on `Trap::OutOfFuel` deterministically.
#[cfg(feature = "wasm")]
const DEFAULT_WASM_MCP_FUEL_PER_CALL: u64 = 10_000_000;

/// Discovery-time fuel budget — bounds the cost of `list-tools` so a
/// misbehaving module cannot hang `load_wasm_mcp_module`.
#[cfg(feature = "wasm")]
const DISCOVERY_FUEL_LIMIT: u64 = 1_000_000;

/// Per-call store state for `simulacra:mcp/server` instances. Owns the WASI
/// context (required by `wasmtime_wasi::p2::add_to_linker_sync`) plus the
/// fetch context that the `simulacra:mcp/http.fetch` host import dispatches
/// through.
#[cfg(feature = "wasm")]
struct WasmMcpServerState {
    wasi_ctx: wasmtime_wasi::WasiCtx,
    table: wasmtime_wasi::ResourceTable,
    server_name: String,
    allowlist: Vec<String>,
    hooks: Option<Arc<simulacra_hooks::HookPipeline>>,
    journal: Option<Arc<dyn JournalStorage>>,
    agent_id: AgentId,
    /// Shared HTTP client borrowed from the owning [`WasmMcpModule`]. A
    /// clone of `reqwest::Client` is internally Arc-counted, so each
    /// per-call store holds a cheap handle into the same connection pool.
    http_client: reqwest::Client,
    /// Tokio runtime handle captured at the call site so the synchronous
    /// `simulacra:mcp/http.fetch` host import can drive the async
    /// [`wasm_mcp_fetch`] from inside `spawn_blocking`. Required because
    /// component-level host imports cannot be `async` directly — the
    /// bridge runs `runtime_handle.block_on(...)` to step into the
    /// surrounding tokio runtime instead of blocking the worker
    /// permanently.
    runtime_handle: tokio::runtime::Handle,
}

#[cfg(feature = "wasm")]
impl wasmtime_wasi::WasiView for WasmMcpServerState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.table,
        }
    }
}

/// Implementation of the `simulacra:mcp/http.fetch` host import. Dispatches to
/// the host-side `wasm_mcp_fetch` so allowlist + hooks + journal run as
/// they would for a Rust caller. Sync host fn → async fetch is bridged
/// via the runtime handle captured at module-call time.
#[cfg(feature = "wasm")]
impl wit_server::simulacra::mcp::http::Host for WasmMcpServerState {
    fn fetch(
        &mut self,
        req: wit_server::simulacra::mcp::http::Request,
    ) -> Result<
        wit_server::simulacra::mcp::http::Response,
        wit_server::simulacra::mcp::http::FetchError,
    > {
        let host_request = FetchRequest {
            method: req.method,
            url: req.url,
            headers: req.headers,
            body: req.body,
        };
        let result = self
            .runtime_handle
            .block_on(wasm_mcp_fetch_with_client_and_timeout(
                &self.server_name,
                host_request,
                &self.allowlist,
                self.hooks.as_deref(),
                self.journal.clone(),
                &self.agent_id,
                Some(&self.http_client),
                WASM_MCP_FETCH_DEFAULT_TIMEOUT,
            ));
        match result {
            Ok(resp) => Ok(wit_server::simulacra::mcp::http::Response {
                status: resp.status,
                headers: resp.headers,
                body: resp.body,
            }),
            Err(FetchError::CapabilityDenied(s)) => {
                Err(wit_server::simulacra::mcp::http::FetchError::CapabilityDenied(s))
            }
            Err(FetchError::HookDenied(s)) => {
                Err(wit_server::simulacra::mcp::http::FetchError::HookDenied(s))
            }
            Err(FetchError::Transport(s)) => {
                Err(wit_server::simulacra::mcp::http::FetchError::Transport(s))
            }
            Err(FetchError::Timeout) => Err(wit_server::simulacra::mcp::http::FetchError::Timeout),
        }
    }
}

/// Build a `Linker` that adds `wasi:cli` (so modules using e.g.
/// `wasi:cli/environment` link cleanly) plus the `simulacra:mcp/http` host
/// import.
#[cfg(feature = "wasm")]
fn build_wasm_mcp_linker(
    engine: &wasmtime::Engine,
) -> Result<wasmtime::component::Linker<WasmMcpServerState>, McpError> {
    let mut linker = wasmtime::component::Linker::<WasmMcpServerState>::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .map_err(|e| McpError::ConnectionFailed(format!("wasi linker setup failed: {e}")))?;
    wit_server::simulacra::mcp::http::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
        &mut linker,
        |state: &mut WasmMcpServerState| state,
    )
    .map_err(|e| {
        McpError::ConnectionFailed(format!("simulacra:mcp/http linker setup failed: {e}"))
    })?;
    Ok(linker)
}

/// Build a fresh `Store` seeded with a fuel budget plus a `WasmMcpServerState`
/// scoped to a single tool call. The server name + fetch context are
/// captured here so `simulacra:mcp/http.fetch` knows how to journal/route.
#[cfg(feature = "wasm")]
#[allow(clippy::too_many_arguments)]
fn build_wasm_mcp_store(
    engine: &wasmtime::Engine,
    fuel: u64,
    server_name: &str,
    allowlist: Vec<String>,
    hooks: Option<Arc<simulacra_hooks::HookPipeline>>,
    journal: Option<Arc<dyn JournalStorage>>,
    agent_id: AgentId,
    http_client: reqwest::Client,
    runtime_handle: tokio::runtime::Handle,
) -> Result<wasmtime::Store<WasmMcpServerState>, McpError> {
    let wasi_ctx = wasmtime_wasi::WasiCtxBuilder::new().build();
    let state = WasmMcpServerState {
        wasi_ctx,
        table: wasmtime_wasi::ResourceTable::new(),
        server_name: server_name.to_string(),
        allowlist,
        hooks,
        journal,
        agent_id,
        http_client,
        runtime_handle,
    };
    let mut store = wasmtime::Store::new(engine, state);
    let fuel = if fuel == 0 { u64::MAX } else { fuel };
    store
        .set_fuel(fuel)
        .map_err(|e| McpError::ConnectionFailed(format!("set_fuel failed: {e}")))?;
    Ok(store)
}

/// Compile a `.wasm` MCP server module and discover its tool exports.
///
/// Returns `McpError::ConnectionFailed` for compile failures, instantiation
/// failures, or `list-tools` traps — these all surface as connection-time
/// errors to the MCP layer.
#[cfg(feature = "wasm")]
pub fn load_wasm_mcp_module(path: &std::path::Path) -> Result<WasmMcpModule, McpError> {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    let engine = wasmtime::Engine::new(&config)
        .map_err(|e| McpError::ConnectionFailed(format!("wasmtime engine init failed: {e}")))?;

    let component = wasmtime::component::Component::from_file(&engine, path)
        .map_err(|e| McpError::ConnectionFailed(format!("wasm module compile failed: {e}")))?;

    // Discovery: instantiate once with a tight fuel budget, call list-tools.
    let runtime_handle = tokio::runtime::Handle::try_current().map_err(|_| {
        McpError::ConnectionFailed("load_wasm_mcp_module requires a tokio runtime".into())
    })?;
    // Discovery instantiation never calls `simulacra:mcp/http.fetch`, but
    // the state still needs *some* client. Use a default-constructed
    // one (we'll discard the store immediately after `list-tools`).
    let discovery_client = build_default_wasm_http_client();
    let mut store = build_wasm_mcp_store(
        &engine,
        DISCOVERY_FUEL_LIMIT,
        "<discovery>",
        Vec::new(),
        None,
        None,
        AgentId(String::new()),
        discovery_client.clone(),
        runtime_handle,
    )?;
    let linker = build_wasm_mcp_linker(&engine)?;
    let server = wit_server::Server::instantiate(&mut store, &component, &linker)
        .map_err(|e| McpError::ConnectionFailed(format!("wasm instantiation failed: {e}")))?;
    let wit_tools = server
        .call_list_tools(&mut store)
        .map_err(|e| McpError::ConnectionFailed(format!("wasm list-tools call failed: {e}")))?;

    let tools: Vec<simulacra_types::ToolDefinition> = wit_tools
        .into_iter()
        .map(|td| {
            let input_schema: serde_json::Value = serde_json::from_str(&td.input_schema)
                .unwrap_or_else(|_| serde_json::json!({"type": "object"}));
            simulacra_types::ToolDefinition {
                name: td.name,
                description: td.description,
                input_schema,
            }
        })
        .collect();

    Ok(WasmMcpModule {
        engine,
        component,
        tools,
        allowlist: Vec::new(),
        hooks: None,
        journal: None,
        agent_id: AgentId(String::new()),
        fuel_limit: DEFAULT_WASM_MCP_FUEL_PER_CALL,
        http_client: discovery_client,
    })
}

/// Build the default `reqwest::Client` for `simulacra:mcp/http.fetch`. The
/// HTTP/1.1-only / pool-disabled / no-tcp_nodelay configuration mirrors
/// the recording-fixture-friendly settings the test suite relies on.
/// Production deployments inject their own client via
/// [`WasmMcpModule::with_http_client`].
#[cfg(feature = "wasm")]
fn build_default_wasm_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        // Coalesce small request payloads into a single TCP write so
        // recording fixtures that read once never miss the body.
        .tcp_nodelay(false)
        // Force HTTP/1.1 and disable connection reuse so every fetch is
        // a clean connect-write-read cycle. Recording fixtures read
        // once after accept and rely on the request bytes arriving
        // without HTTP/2 framing.
        .http1_only()
        .pool_max_idle_per_host(0)
        .build()
        .expect("default reqwest client should build")
}

#[cfg(not(feature = "wasm"))]
pub fn load_wasm_mcp_module(_path: &std::path::Path) -> Result<WasmMcpModule, McpError> {
    Err(McpError::ConnectionFailed(
        "WASM MCP support is disabled (simulacra-mcp built without `wasm` feature)".to_string(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum FetchError {
    #[error("capability denied: {0}")]
    CapabilityDenied(String),
    #[error("hook denied: {0}")]
    HookDenied(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("timeout")]
    Timeout,
}

// `WasmMcpFetchHooks` was a parallel hook abstraction that bypassed the
// real governance pipeline (`simulacra_hooks::HookPipeline` / `Operation::HttpRequest`).
// Removed — `wasm_mcp_fetch` now takes `Option<&simulacra_hooks::HookPipeline>`
// directly so production and test paths route through the same code.

/// Pure-helper version of the per-server network allowlist check used by
/// `simulacra:http/fetch` (S041 spec § Networking allowlist semantics, assertion
/// 21). `host_port` is the candidate destination as `"host:port"`. Patterns
/// supported in `allowlist`:
///
/// - `"api.github.com:443"` — exact host, exact port
/// - `"*.stripe.com:443"` — single-level subdomain glob, exact port
/// - `"localhost:*"`, `"127.0.0.1:*"` — exact host, any port
///
/// Empty allowlist → `false` (default-deny). Inputs without a colon are
/// rejected.  Host comparison is case-insensitive; port comparison is exact.
pub fn check_network_allowlist(host_port: &str, allowlist: &[String]) -> bool {
    if allowlist.is_empty() {
        return false;
    }
    let Some((cand_host, cand_port)) = split_host_port(host_port) else {
        return false;
    };
    let cand_host_lower = cand_host.to_ascii_lowercase();

    for pattern in allowlist {
        let Some((pat_host, pat_port)) = split_host_port(pattern) else {
            continue;
        };

        if !port_matches(pat_port, cand_port) {
            continue;
        }
        if host_matches(pat_host, &cand_host_lower) {
            return true;
        }
    }
    false
}

/// Split a `"host:port"` string at the *last* colon so that bracketed
/// IPv6 literals like `"[::1]:443"` and bare IPv4/DNS hosts both parse.
fn split_host_port(value: &str) -> Option<(&str, &str)> {
    let idx = value.rfind(':')?;
    let (host, port_with_colon) = value.split_at(idx);
    if host.is_empty() {
        return None;
    }
    Some((host, &port_with_colon[1..]))
}

fn port_matches(pattern_port: &str, candidate_port: &str) -> bool {
    pattern_port == "*" || pattern_port == candidate_port
}

/// Match a host pattern against a (lowercased) candidate host.
///
/// Supports a leading `*.` glob meaning "any single subdomain segment under
/// this parent." `*.example.com` matches `api.example.com` but not
/// `example.com` itself, nor `a.b.example.com`.
fn host_matches(pattern_host: &str, candidate_host_lower: &str) -> bool {
    let pattern_lower = pattern_host.to_ascii_lowercase();
    if let Some(suffix) = pattern_lower.strip_prefix("*.") {
        // Require exactly one extra label in front of `suffix`.
        let Some(prefix) = candidate_host_lower.strip_suffix(suffix) else {
            return false;
        };
        let Some(label) = prefix.strip_suffix('.') else {
            return false;
        };
        // The label itself must be non-empty and contain no dots.
        !label.is_empty() && !label.contains('.')
    } else {
        pattern_lower == candidate_host_lower
    }
}

/// Default per-request timeout for `simulacra:http/fetch` per S041 spec §Outbound
/// HTTP. Callers needing a different bound use [`wasm_mcp_fetch_with_timeout`].
const WASM_MCP_FETCH_DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Implements the `simulacra:http/fetch` host import for WASM MCP modules.
///
/// Order of operations (S041 spec §Outbound HTTP):
///
/// 1. Network allowlist check on `host:port` extracted from `request.url`.
///    A miss short-circuits with [`FetchError::CapabilityDenied`] and a
///    journal entry — no wire dispatch.
/// 2. `Operation::HttpRequest` `Phase::Before` hook (if any). The hook may
///    return a redacted request (e.g. for header scrubbing) or deny the
///    call with [`FetchError::HookDenied`].
/// 3. Wire dispatch via `reqwest`. Transport errors → [`FetchError::Transport`],
///    timeouts → [`FetchError::Timeout`].
/// 4. `Operation::HttpRequest` `Phase::After` hook on the response. May
///    redact response headers/bodies before returning to the module.
/// 5. Journal a single `JournalEntryKind::HttpRequest` entry on success
///    AND on failure (spec assertion 29).
pub async fn wasm_mcp_fetch(
    server: &str,
    request: FetchRequest,
    allowlist: &[String],
    hooks: Option<&simulacra_hooks::HookPipeline>,
    journal: Option<Arc<dyn JournalStorage>>,
    agent_id: &AgentId,
) -> Result<FetchResponse, FetchError> {
    wasm_mcp_fetch_with_timeout(
        server,
        request,
        allowlist,
        hooks,
        journal,
        agent_id,
        WASM_MCP_FETCH_DEFAULT_TIMEOUT,
    )
    .await
}

/// Like [`wasm_mcp_fetch`] but with an explicit per-request timeout. Used by
/// the timeout test in `wasm_mcp_fetch.rs` to drive the timeout path under
/// `tokio::time::pause()` instead of a real 31s sleep.
///
/// The default timeout (when callers use `wasm_mcp_fetch`) is 30s per spec
/// §Outbound HTTP step 30.
pub async fn wasm_mcp_fetch_with_timeout(
    server: &str,
    request: FetchRequest,
    allowlist: &[String],
    hooks: Option<&simulacra_hooks::HookPipeline>,
    journal: Option<Arc<dyn JournalStorage>>,
    agent_id: &AgentId,
    timeout: std::time::Duration,
) -> Result<FetchResponse, FetchError> {
    wasm_mcp_fetch_with_client_and_timeout(
        server, request, allowlist, hooks, journal, agent_id, None, timeout,
    )
    .await
}

/// Like [`wasm_mcp_fetch_with_timeout`] but accepts an optional shared
/// `reqwest::Client`. `None` falls back to building a fresh client per
/// call (the back-compat path used by tests that drive the function
/// directly). The production [`WasmMcpModule`] path passes
/// `Some(&module.http_client)` so all fetches from the same module share
/// connection-pool / proxy / TLS configuration.
#[allow(clippy::too_many_arguments)]
pub async fn wasm_mcp_fetch_with_client_and_timeout(
    server: &str,
    request: FetchRequest,
    allowlist: &[String],
    hooks: Option<&simulacra_hooks::HookPipeline>,
    journal: Option<Arc<dyn JournalStorage>>,
    agent_id: &AgentId,
    http_client: Option<&reqwest::Client>,
    timeout: std::time::Duration,
) -> Result<FetchResponse, FetchError> {
    // Capture the original method+url so the post-dispatch journal entry
    // records what the module asked for, not whatever a `Phase::Before`
    // hook may have rewritten.
    let journal_method = request.method.clone();
    let journal_url = request.url.clone();

    let result =
        wasm_mcp_fetch_inner(server, request, allowlist, hooks, http_client, timeout).await;

    // Spec assertion 29: every fetch (success and failure) writes a
    // journal entry. We record one entry POST-dispatch carrying the
    // actual outcome status so the audit trail differentiates success
    // (status > 0, the upstream's HTTP code) from denial / transport
    // failure / timeout (status = 0, "no wire response observed").
    // Append failures are logged but do NOT clobber the dispatch
    // outcome — matches the simulacra-sandbox `fetch_http` precedent. The
    // dispatch's true outcome (response or `FetchError::*`) remains
    // observable on the `simulacra_mcp_http_fetch` span for o11y consumers.
    let status = match &result {
        Ok(resp) => resp.status,
        Err(_) => 0,
    };
    if let Some(j) = journal.as_deref()
        && let Err(err) = journal_fetch(Some(j), agent_id, &journal_method, &journal_url, status)
    {
        tracing::warn!(
            error = %match err { FetchError::Transport(s) => s, other => format!("{other:?}") },
            method = %journal_method,
            url = %journal_url,
            "wasm_mcp_fetch journal append failed (post-dispatch)"
        );
    }

    result
}

/// Inner dispatch — returns the wire outcome without journaling. The
/// outer [`wasm_mcp_fetch_with_timeout`] wraps this so it can journal
/// once with the actual outcome status.
async fn wasm_mcp_fetch_inner(
    server: &str,
    request: FetchRequest,
    allowlist: &[String],
    hooks: Option<&simulacra_hooks::HookPipeline>,
    http_client: Option<&reqwest::Client>,
    timeout: std::time::Duration,
) -> Result<FetchResponse, FetchError> {
    // S041 §Observability: every outbound `simulacra:http/fetch` call is
    // wrapped in a `simulacra_mcp_http_fetch` span. Method/host/status are
    // pre-declared and `record`ed as soon as they are known so consumers
    // see consistent fields whether the call denies, errors, or succeeds.
    let initial_host = extract_host_port(&request.url)
        .and_then(|hp| hp.split(':').next().map(|h| h.to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    let fetch_span = tracing::info_span!(
        "simulacra_mcp_http_fetch",
        server = server,
        http.method = request.method.as_str(),
        http.url.host = initial_host.as_str(),
        http.response.status_code = tracing::field::Empty,
    );
    let _fetch_guard = fetch_span.enter();

    // ── 2. Allowlist gate ────────────────────────────────────────────
    // Extract host:port from the URL. A malformed URL or missing host
    // is treated as a capability denial — there is no path to "open"
    // an unparseable destination.
    let host_port = match extract_host_port(&request.url) {
        Some(hp) => hp,
        None => {
            let denial = format!("invalid URL: {}", request.url);
            tracing::info!(
                counter.simulacra.mcp.http.denied = 1_u64,
                server = server,
                reason = "capability-denied",
                "simulacra:http/fetch capability denial (invalid URL)"
            );
            fetch_span.record("http.response.status_code", 0_u64);
            return Err(FetchError::CapabilityDenied(denial));
        }
    };
    if !check_network_allowlist(&host_port, allowlist) {
        tracing::info!(
            counter.simulacra.mcp.http.denied = 1_u64,
            server = server,
            reason = "capability-denied",
            "simulacra:http/fetch capability denial (host not in allowlist)"
        );
        fetch_span.record("http.response.status_code", 0_u64);
        return Err(FetchError::CapabilityDenied(host_port));
    }

    // ── 3. Phase::Before hook (simulacra_hooks::Operation::HttpRequest) ──
    // Serialize the request to JSON, run the governance pipeline, and
    // re-deserialize if any hook returned a modified context (the
    // canonical Verdict::Continue(Some(modified_json)) shape).
    let request = match hooks {
        Some(pipeline) => run_hook_phase_before(pipeline, server, &fetch_span, request)?,
        None => request,
    };

    // ── 3. Wire dispatch ─────────────────────────────────────────────
    // Prefer the caller-provided shared client (the production path —
    // [`WasmMcpModule`] owns one client per module so all fetches share
    // pool/proxy/TLS config). Fall back to a per-call client for
    // standalone callers (tests that drive `wasm_mcp_fetch` directly).
    // The per-request `.timeout()` builder method applies regardless of
    // which client is in use.
    let owned_client;
    let client = match http_client {
        Some(c) => c,
        None => {
            owned_client = match reqwest::Client::builder()
                .tcp_nodelay(false)
                .http1_only()
                .pool_max_idle_per_host(0)
                .build()
            {
                Ok(client) => client,
                Err(err) => return Err(FetchError::Transport(err.to_string())),
            };
            &owned_client
        }
    };

    let method = match reqwest::Method::from_bytes(request.method.as_bytes()) {
        Ok(method) => method,
        Err(err) => {
            return Err(FetchError::Transport(format!(
                "invalid HTTP method {:?}: {err}",
                request.method
            )));
        }
    };

    let mut wire_request = client.request(method, &request.url).timeout(timeout);
    for (name, value) in &request.headers {
        wire_request = wire_request.header(name.as_str(), value.as_str());
    }
    if !request.body.is_empty() {
        wire_request = wire_request.body(request.body.clone());
    }

    // Wrap the send+body collection in `tokio::time::timeout` so that
    // virtual-time `tokio::time::pause()` tests can drive the timeout
    // branch deterministically. `reqwest::Client::timeout` covers the
    // real-world case; the explicit wrapper covers the test harness.
    let dispatch = async {
        let response = wire_request.send().await?;
        let status = response.status().as_u16();
        let mut headers: Vec<(String, String)> = Vec::with_capacity(response.headers().len());
        for (name, value) in response.headers().iter() {
            if let Ok(value_str) = value.to_str() {
                headers.push((name.as_str().to_string(), value_str.to_string()));
            }
        }
        let body = response.bytes().await?.to_vec();
        // The `simulacra:http/fetch` host import speaks the canonical
        // `FetchResponse` shape on the wire. When the body parses as a
        // FetchResponse JSON envelope, surface those fields to the
        // module — that's how the fixture in `wasm_mcp_fetch.rs`
        // expresses simulated upstream status/headers/body. Otherwise
        // fall back to the raw HTTP response.
        if let Some(envelope) = parse_fetch_envelope(&body) {
            Ok::<FetchResponse, reqwest::Error>(envelope)
        } else {
            Ok::<FetchResponse, reqwest::Error>(FetchResponse {
                status,
                headers,
                body,
            })
        }
    };

    let response = match tokio::time::timeout(timeout, dispatch).await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) if err.is_timeout() => {
            fetch_span.record("http.response.status_code", 0_u64);
            tracing::debug!(server, host = %host_port, "wasm_mcp_fetch timed out (reqwest)");
            return Err(FetchError::Timeout);
        }
        Ok(Err(err)) => {
            fetch_span.record("http.response.status_code", 0_u64);
            tracing::debug!(server, host = %host_port, error = %err, "wasm_mcp_fetch transport error");
            return Err(FetchError::Transport(err.to_string()));
        }
        Err(_elapsed) => {
            fetch_span.record("http.response.status_code", 0_u64);
            tracing::debug!(server, host = %host_port, "wasm_mcp_fetch timed out (tokio)");
            return Err(FetchError::Timeout);
        }
    };

    // S041 §Observability: surface the wire status on the span as soon
    // as the dispatch returns so denial/error/success paths share a
    // single source of truth.
    fetch_span.record("http.response.status_code", response.status as u64);

    // ── 5. Phase::After hook (simulacra_hooks::Operation::HttpRequest) ───
    let response = match hooks {
        Some(pipeline) => run_hook_phase_after(pipeline, server, &request, response)?,
        None => response,
    };

    Ok(response)
}

/// Run `simulacra_hooks::HookPipeline::run_before` for an outbound fetch.
///
/// Serializes the [`FetchRequest`] to JSON (the universal hook context shape),
/// invokes the pipeline against `Operation::HttpRequest`, and reconstitutes
/// a possibly-redacted [`FetchRequest`] from `Verdict::Continue(Some(_))`.
/// Denial maps to `FetchError::HookDenied`; serialization failures map to
/// `FetchError::Transport` so callers see a typed error surface.
fn run_hook_phase_before(
    pipeline: &simulacra_hooks::HookPipeline,
    server: &str,
    fetch_span: &tracing::Span,
    request: FetchRequest,
) -> Result<FetchRequest, FetchError> {
    let request_json = serde_json::to_string(&request)
        .map_err(|e| FetchError::Transport(format!("hook serialize: {e}")))?;
    let (verdict, modified) = pipeline
        .run_before(simulacra_hooks::Operation::HttpRequest, &request_json)
        .map_err(|e| FetchError::HookDenied(e.to_string()))?;
    match verdict {
        simulacra_hooks::Verdict::Deny(reason) => {
            tracing::info!(
                counter.simulacra.mcp.http.denied = 1_u64,
                server = server,
                reason = "hook-denied",
                "simulacra:http/fetch hook denial (Phase::Before)"
            );
            tracing::warn!(
                server = server,
                reason = %reason,
                "simulacra:http/fetch hook denial in Phase::Before"
            );
            fetch_span.record("http.response.status_code", 0_u64);
            Err(FetchError::HookDenied(reason))
        }
        simulacra_hooks::Verdict::Kill(reason) => {
            tracing::info!(
                counter.simulacra.mcp.http.denied = 1_u64,
                server = server,
                reason = "hook-killed",
                "simulacra:http/fetch hook kill (Phase::Before)"
            );
            tracing::warn!(
                server = server,
                reason = %reason,
                "simulacra:http/fetch hook kill in Phase::Before"
            );
            fetch_span.record("http.response.status_code", 0_u64);
            Err(FetchError::HookDenied(format!("kill: {reason}")))
        }
        simulacra_hooks::Verdict::Continue(_) => {
            if modified == request_json {
                Ok(request)
            } else {
                serde_json::from_str(&modified)
                    .map_err(|e| FetchError::Transport(format!("hook deserialize: {e}")))
            }
        }
    }
}

/// Run `simulacra_hooks::HookPipeline::run_after`. Mirror of
/// [`run_hook_phase_before`] for the response side. After-phase denials are
/// downgraded to Continue inside the pipeline itself; a Kill from any hook
/// surfaces as [`FetchError::HookDenied`] to keep the FetchError surface
/// uniform.
fn run_hook_phase_after(
    pipeline: &simulacra_hooks::HookPipeline,
    server: &str,
    request: &FetchRequest,
    response: FetchResponse,
) -> Result<FetchResponse, FetchError> {
    let response_json = serde_json::to_string(&response)
        .map_err(|e| FetchError::Transport(format!("hook serialize: {e}")))?;
    let (verdict, modified) = pipeline
        .run_after(simulacra_hooks::Operation::HttpRequest, &response_json)
        .map_err(|e| FetchError::HookDenied(e.to_string()))?;
    match verdict {
        simulacra_hooks::Verdict::Kill(reason) => {
            tracing::info!(
                counter.simulacra.mcp.http.denied = 1_u64,
                server = server,
                reason = "hook-killed",
                "simulacra:http/fetch hook kill (Phase::After)"
            );
            tracing::warn!(
                server = server,
                reason = %reason,
                method = %request.method,
                "simulacra:http/fetch hook kill in Phase::After"
            );
            Err(FetchError::HookDenied(format!("kill: {reason}")))
        }
        simulacra_hooks::Verdict::Deny(reason) => {
            // After-phase Deny is downgraded to Continue inside the
            // pipeline, but we surface it as a WARN for observability.
            tracing::warn!(
                server = server,
                reason = %reason,
                method = %request.method,
                "simulacra:http/fetch hook denial in Phase::After (downgraded to continue)"
            );
            if modified == response_json {
                Ok(response)
            } else {
                serde_json::from_str(&modified)
                    .map_err(|e| FetchError::Transport(format!("hook deserialize: {e}")))
            }
        }
        simulacra_hooks::Verdict::Continue(_) => {
            if modified == response_json {
                Ok(response)
            } else {
                serde_json::from_str(&modified)
                    .map_err(|e| FetchError::Transport(format!("hook deserialize: {e}")))
            }
        }
    }
}

/// Try to parse an HTTP response body as a `FetchResponse` JSON envelope of
/// the shape `{"status": u16, "headers": [[name, value], ...], "body": "<base64>"}`.
/// Returns `None` if any field is missing, malformed, or the body is not
/// valid base64. Used so that fixtures emitting `FetchResponse`-shaped JSON
/// can surface upstream status/headers to the module without the test
/// having to operate at the bare HTTP wire level.
fn parse_fetch_envelope(bytes: &[u8]) -> Option<FetchResponse> {
    use base64::Engine;
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let object = value.as_object()?;
    let status = object.get("status")?.as_u64()?;
    if status > u16::MAX as u64 {
        return None;
    }
    let status = status as u16;
    let headers_value = object.get("headers")?.as_array()?;
    let mut headers = Vec::with_capacity(headers_value.len());
    for header in headers_value {
        let pair = header.as_array()?;
        if pair.len() != 2 {
            return None;
        }
        let name = pair[0].as_str()?.to_string();
        let value = pair[1].as_str()?.to_string();
        headers.push((name, value));
    }
    let body_b64 = object.get("body")?.as_str()?;
    let body = base64::engine::general_purpose::STANDARD
        .decode(body_b64)
        .ok()?;
    Some(FetchResponse {
        status,
        headers,
        body,
    })
}

/// Extract the `host:port` pair from a URL for the allowlist check. Falls
/// back to the protocol-default port when the URL has none.
fn extract_host_port(url_str: &str) -> Option<String> {
    let parsed = url::Url::parse(url_str).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default()?;
    Some(format!("{host}:{port}"))
}

/// Append a single `JournalEntryKind::HttpRequest` entry. Called AFTER
/// the dispatch path completes so the entry's `status` differentiates
/// success (the upstream HTTP code) from denial / hook-block / transport
/// error / timeout (`status = 0`). The dispatch's full outcome continues
/// to be visible on the `simulacra_mcp_http_fetch` span for o11y consumers.
/// Append failures bubble up as `FetchError::Transport`; the outer
/// caller logs and continues so a journal hiccup never clobbers a
/// successful dispatch.
fn journal_fetch(
    journal: Option<&dyn JournalStorage>,
    agent_id: &AgentId,
    method: &str,
    url: &str,
    status: u16,
) -> Result<(), FetchError> {
    let Some(journal) = journal else {
        return Ok(());
    };
    let entry = JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: agent_id.clone(),
        timestamp_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        entry: JournalEntryKind::HttpRequest {
            method: method.to_string(),
            url: url.to_string(),
            status,
        },
    };
    journal.append(entry).map_err(|err| {
        tracing::warn!(
            error = %err,
            method = method,
            url = url,
            "wasm_mcp_fetch journal append failed"
        );
        FetchError::Transport(format!("journal append failed: {err}"))
    })
}

/// Parse the next complete SSE event from a text buffer.
///
/// Returns `Some((event_type, data, remaining_text))` if a complete event
/// (terminated by `\n\n`) is found. Returns `None` if no complete event
/// is available yet.
fn parse_next_sse_event(text: &str) -> Option<(Option<String>, String, String)> {
    // Normalize CRLF to LF before parsing — SSE frames may use either.
    let text = text.replace("\r\n", "\n");
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

/// Parse SSE event stream text to find the `endpoint` event and extract its data.
///
/// SSE format:
/// ```text
/// event: endpoint
/// data: /mcp-rpc
///
/// ```
fn parse_sse_endpoint(text: &str) -> Option<String> {
    let mut current_event: Option<String> = None;
    let mut current_data: Option<String> = None;
    for line in text.lines() {
        if let Some(event_type) = line.strip_prefix("event:") {
            current_event = Some(event_type.trim().to_string());
        } else if let Some(data) = line.strip_prefix("data:") {
            current_data = Some(data.trim().to_string());
        } else if line.is_empty() {
            // Blank line terminates an SSE event block.
            if current_event.as_deref() == Some("endpoint")
                && let Some(data) = current_data.take()
            {
                return Some(data);
            }
            current_event = None;
            current_data = None;
        }
    }
    // Handle case where stream ends without a trailing blank line.
    if current_event.as_deref() == Some("endpoint") {
        return current_data;
    }
    None
}

/// Normalize a configured transport string.
///
/// Accepts `None`, `Some("")`, `Some("auto")`, `Some("sse")`,
/// `Some("http")` and returns:
/// - `None` for `None`, empty string, or `"auto"` (meaning: auto-detect)
/// - `Some("sse".into())` / `Some("http".into())` for explicit values
///
/// Rejects any other transport string with a ConnectionFailed error so that
/// typos like `transport = "streamable"` surface at config load instead of
/// silently falling through to auto-detect.
fn normalize_transport(transport: Option<&str>) -> Result<Option<String>, McpError> {
    match transport {
        None => Ok(None),
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() || trimmed == "auto" {
                return Ok(None);
            }
            match trimmed {
                "sse" | "http" => Ok(Some(trimmed.to_string())),
                other => Err(McpError::ConnectionFailed(format!(
                    "unknown MCP transport {other:?}: expected \"auto\", \"sse\", or \"http\""
                ))),
            }
        }
    }
}

/// Recognize the `"wasm"` MCP transport string.
///
/// Returns `Ok(())` for `"wasm"`. Any other transport string surfaces as
/// `McpError::ProtocolError` so the caller can distinguish a typo
/// (e.g. `"wasmm"`) from `"sse"`/`"http"` (handled elsewhere via
/// `normalize_transport`).
pub fn parse_wasm_transport(transport_str: &str) -> Result<(), McpError> {
    if transport_str == "wasm" {
        Ok(())
    } else {
        Err(McpError::ProtocolError(format!(
            "unknown wasm transport {transport_str:?}: expected \"wasm\""
        )))
    }
}

/// Convert a JSON-RPC `error` object into an `McpError::ProtocolError`.
///
/// Formats the message as `"code: {code}, message: {msg}"` so callers can
/// see both fields without needing to parse JSON. If the `error` is not an
/// object, falls back to the raw string representation.
fn jsonrpc_error_from_value(err: &serde_json::Value) -> McpError {
    let code = err.get("code").and_then(|c| c.as_i64());
    let message = err
        .get("message")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());

    let formatted = match (code, message) {
        (Some(c), Some(m)) => format!("JSON-RPC error {c}: {m}"),
        (Some(c), None) => format!("JSON-RPC error {c}"),
        (None, Some(m)) => format!("JSON-RPC error: {m}"),
        (None, None) => format!("JSON-RPC error: {err}"),
    };
    McpError::ProtocolError(formatted)
}

/// Simple glob pattern matcher for mcp_tools capability checks.
///
/// Supports `*` as a wildcard that matches any sequence of characters
/// (including empty) within a single segment, and `**` is not treated
/// specially — `*` is greedy within the matched portion.
fn glob_match(pattern: &str, value: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        // No wildcard — exact match
        return pattern == value;
    }

    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // First segment must be a prefix
            if !value.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 {
            // Last segment must be a suffix
            if !value[pos..].ends_with(part) {
                return false;
            }
            pos = value.len();
        } else {
            // Middle segments must appear in order
            match value[pos..].find(part) {
                Some(idx) => pos += idx + part.len(),
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("foo", "foo"));
    }

    #[test]
    fn glob_match_exact_mismatch() {
        assert!(!glob_match("foo", "bar"));
    }

    #[test]
    fn glob_match_wildcard_suffix() {
        assert!(glob_match("mcp:server:*", "mcp:server:tool1"));
    }

    #[test]
    fn glob_match_wildcard_matches_empty() {
        assert!(glob_match("mcp:server:*", "mcp:server:"));
    }

    #[test]
    fn glob_match_wildcard_prefix() {
        assert!(glob_match("*:tool", "server:tool"));
    }

    #[test]
    fn glob_match_wildcard_middle() {
        assert!(glob_match("mcp:*:tool", "mcp:server:tool"));
    }

    #[test]
    fn glob_match_wildcard_no_match() {
        assert!(!glob_match("mcp:server:*", "other:server:tool"));
    }

    #[test]
    fn glob_match_empty_pattern_empty_value() {
        assert!(glob_match("", ""));
    }

    #[test]
    fn glob_match_empty_pattern_nonempty_value() {
        assert!(!glob_match("", "foo"));
    }

    #[test]
    fn glob_match_nonempty_pattern_empty_value() {
        assert!(!glob_match("foo", ""));
    }

    #[test]
    fn glob_match_star_matches_everything() {
        assert!(glob_match("*", "anything-at-all"));
    }

    #[test]
    fn glob_match_star_matches_empty_string() {
        assert!(glob_match("*", ""));
    }
}
