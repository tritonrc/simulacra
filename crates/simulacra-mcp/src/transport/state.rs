use crate::domain::tool_schema::McpToolSchema;

/// Transport mode for an MCP connection, determined during handshake.
#[derive(Debug)]
pub(crate) enum TransportMode {
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
pub(crate) struct McpConnection {
    #[allow(dead_code)]
    pub(crate) server_name: String,
    #[allow(dead_code)]
    pub(crate) url: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) tools: Vec<McpToolSchema>,
    /// Whether the MCP handshake (initialize + tools/list) has been performed.
    pub(crate) handshake_done: bool,
    /// Whether this connection has previously completed a successful handshake.
    /// Used to decide whether to attempt reconnection on failure.
    pub(crate) was_connected: bool,
    /// Transport mode, determined during handshake. None before first handshake.
    pub(crate) transport_mode: Option<TransportMode>,
    /// Configured transport preference from simulacra.toml.
    /// None = auto-detect, Some("sse") = legacy SSE, Some("http") = streamable HTTP.
    pub(crate) configured_transport: Option<String>,
}

impl McpConnection {
    pub(crate) fn new(
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
