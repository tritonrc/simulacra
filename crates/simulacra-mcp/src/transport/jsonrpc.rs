use crate::error::McpError;
use crate::transport::sse::parse_next_sse_event;

/// Read a non-streaming JSON-RPC response that may be plain JSON or a single
/// SSE message, per MCP streamable HTTP.
pub(crate) async fn read_jsonrpc_http_response(
    response: reqwest::Response,
) -> Result<serde_json::Value, McpError> {
    let is_event_stream = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|content_type| content_type.starts_with("text/event-stream"))
        .unwrap_or(false);
    let body = response
        .text()
        .await
        .map_err(|e| McpError::ProtocolError(e.to_string()))?;
    let trimmed = body.trim_start();

    if is_event_stream || trimmed.starts_with("event:") || trimmed.starts_with("data:") {
        let mut rest = body;
        while let Some((_event_type, data, remaining)) = parse_next_sse_event(&rest) {
            rest = remaining;
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) {
                return Ok(value);
            }
        }

        return Err(McpError::ProtocolError(
            "no JSON data event in SSE response".to_string(),
        ));
    }

    serde_json::from_str::<serde_json::Value>(&body)
        .map_err(|e| McpError::ProtocolError(e.to_string()))
}

/// Convert a JSON-RPC `error` object into an `McpError::ProtocolError`.
///
/// Formats the message as `"code: {code}, message: {msg}"` so callers can
/// see both fields without needing to parse JSON. If the `error` is not an
/// object, falls back to the raw string representation.
pub(crate) fn jsonrpc_error_from_value(err: &serde_json::Value) -> McpError {
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
