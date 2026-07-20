use crate::error::McpError;
use crate::observability::McpMeters;
use crate::transport::state::TransportMode;
use opentelemetry::KeyValue;
use simulacra_types::AgentId;

use super::McpManager;

impl McpManager {
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
    pub(crate) async fn dispatch_with_reconnect(
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
            error_kind = %first_err.kind(),
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
                    error_kind = %err.kind(),
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
                        error_kind = %e.kind(),
                        "MCP reconnection attempt failed"
                    );
                    last_err = e;
                }
            }
        }

        tracing::warn!(
            server = %server,
            attempts = MAX_RETRIES,
            error_kind = %last_err.kind(),
            "MCP reconnection exhausted; giving up"
        );
        Err(last_err)
    }

    pub(crate) fn connection_handshake_done(&self, server: &str) -> bool {
        self.connections
            .get(server)
            .map(|c| c.handshake_done)
            .unwrap_or(false)
    }

    pub(crate) fn handshake_failed_error(server: &str) -> McpError {
        McpError::ConnectionFailed(format!("MCP handshake failed for server {server}"))
    }

    /// Check whether an error is a transport-level failure that could be
    /// recovered by reconnecting.
    pub(crate) fn is_transport_error(&self, err: &McpError) -> bool {
        matches!(
            err,
            McpError::TransportError(_) | McpError::ConnectionFailed(_)
        )
    }
}
