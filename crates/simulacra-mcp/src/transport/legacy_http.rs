use crate::domain::tool_schema::McpToolSchema;
use crate::error::McpError;
use crate::manager::McpManager;
use crate::transport::jsonrpc::jsonrpc_error_from_value;

impl McpManager {
    /// Perform the MCP HTTP handshake: initialize then tools/list.
    ///
    /// Returns discovered tools, or an empty vec if the handshake
    /// fails (non-fatal — the connection is still registered for
    /// lazy retry later).
    pub(crate) async fn perform_http_handshake(
        &self,
        url: &str,
    ) -> Result<Vec<McpToolSchema>, McpError> {
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
}
