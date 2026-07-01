use crate::error::McpError;
use crate::manager::McpManager;
use crate::transport::jsonrpc::jsonrpc_error_from_value;

impl McpManager {
    /// Parse an SSE streaming response from a tool call.
    ///
    /// Buffers SSE events, logs progress notifications via `tracing::debug!`,
    /// and extracts the final JSON-RPC result. Returns `ProtocolError` if the
    /// stream closes without delivering a result, or `TransportError` on 60s
    /// idle timeout.
    pub(crate) async fn parse_sse_tool_response(
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

/// Parse the next complete SSE event from a text buffer.
///
/// Returns `Some((event_type, data, remaining_text))` if a complete event
/// (terminated by `\n\n`) is found. Returns `None` if no complete event
/// is available yet.
pub(crate) fn parse_next_sse_event(text: &str) -> Option<(Option<String>, String, String)> {
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
pub(crate) fn parse_sse_endpoint(text: &str) -> Option<String> {
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
