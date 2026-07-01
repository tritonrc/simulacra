use crate::error::McpError;

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
pub(crate) fn normalize_transport(transport: Option<&str>) -> Result<Option<String>, McpError> {
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
