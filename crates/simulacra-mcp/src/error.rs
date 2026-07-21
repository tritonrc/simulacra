/// Errors from MCP operations.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("protocol error: {0}")]
    ProtocolError(String),

    #[error("transport error: {0}")]
    TransportError(String),

    /// Upstream rejected the credential on the HTTP dispatch path.
    #[error("auth failed: {0}")]
    AuthFailed(String),

    /// Response body exceeded the configured read cap while streaming.
    #[error("response too large: exceeded {limit_bytes} bytes")]
    ResponseTooLarge { limit_bytes: usize },

    #[error("capability denied: {0}")]
    CapabilityDenied(String),
}

impl McpError {
    /// A stable, non-sensitive discriminant for structured logging.
    ///
    /// Distinguishes dispatch authentication failures from handshake connection
    /// failures and transport/route failures, so a log can say *why* a request
    /// failed without emitting the detail string, which may carry sensitive
    /// URLs, headers, or credentials.
    pub fn kind(&self) -> &'static str {
        match self {
            McpError::ConnectionFailed(_) => "connection_failed",
            McpError::ProtocolError(_) => "protocol_error",
            McpError::TransportError(_) => "transport_error",
            McpError::AuthFailed(_) => "auth_failed",
            McpError::ResponseTooLarge { .. } => "response_too_large",
            McpError::CapabilityDenied(_) => "capability_denied",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::McpError;

    #[test]
    fn kind_is_the_variant_discriminant_not_the_detail() {
        // The detail string (which may hold a token) must never appear in `kind`.
        assert_eq!(
            McpError::ConnectionFailed("Bearer ghs_secret 401".into()).kind(),
            "connection_failed"
        );
        assert_eq!(
            McpError::TransportError("404".into()).kind(),
            "transport_error"
        );
        assert_eq!(McpError::ProtocolError("x".into()).kind(), "protocol_error");
        assert_eq!(
            McpError::CapabilityDenied("x".into()).kind(),
            "capability_denied"
        );
    }
}
