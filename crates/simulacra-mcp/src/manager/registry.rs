use crate::domain::tool_schema::bridge_tool_schema;
use crate::domain::transport_config::normalize_transport;
use crate::error::McpError;
use crate::transport::state::McpConnection;
use simulacra_types::ToolDefinition;

use super::McpManager;

impl McpManager {
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
    pub(crate) async fn ensure_server_connected(&mut self, server: &str) {
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

    /// List tool definitions from all connected MCP servers.
    ///
    /// Triggers the lazy MCP handshake for any servers that have not
    /// yet been initialized, then aggregates all discovered tools.
    pub async fn list_tools(&mut self) -> Vec<ToolDefinition> {
        self.ensure_connected().await;
        self.connections
            .values()
            .flat_map(|conn| conn.tools.iter().map(bridge_tool_schema))
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
                    .map(|t| (server_name.clone(), bridge_tool_schema(t)))
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
}
