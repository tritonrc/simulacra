use crate::error::McpError;
use crate::transport::headers::apply_connection_headers;
use crate::transport::jsonrpc::jsonrpc_error_from_value;
use crate::transport::state::TransportMode;
use simulacra_types::AgentId;

use super::McpManager;

impl McpManager {
    /// For StreamableHttp mode:
    /// - Sends `Accept: application/json, text/event-stream` and `Mcp-Session-Id`.
    /// - Checks for HTTP 404 with a stored session ID → returns a session-expired
    ///   `ProtocolError` so `dispatch_with_reconnect` can handle it.
    /// - Branches on response `Content-Type`: JSON is parsed directly, SSE is
    ///   streamed via `parse_sse_tool_response`.
    pub(crate) async fn dispatch_tool_call(
        &self,
        agent_id: &AgentId,
        server: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        #[cfg(not(feature = "wasm"))]
        let _ = agent_id;

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
}
