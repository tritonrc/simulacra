use crate::domain::tool_schema::McpToolSchema;
use crate::error::McpError;
use crate::manager::McpManager;
use crate::transport::headers::apply_connection_headers;
use crate::transport::jsonrpc::{jsonrpc_error_from_value, read_jsonrpc_http_response};

impl McpManager {
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
    pub(crate) async fn perform_streamable_http_handshake(
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
        let init_body: serde_json::Value = read_jsonrpc_http_response(init_response).await?;

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
        let body: serde_json::Value = read_jsonrpc_http_response(tools_response).await?;

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
}
