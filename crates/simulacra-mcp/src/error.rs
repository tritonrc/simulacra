/// Errors from MCP operations.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("protocol error: {0}")]
    ProtocolError(String),

    #[error("transport error: {0}")]
    TransportError(String),

    #[error("capability denied: {0}")]
    CapabilityDenied(String),
}

impl McpError {
    /// A stable, non-sensitive discriminant for structured logging.
    ///
    /// Distinguishes an auth/connection failure — an expired credential
    /// surfaces as a 401/403 mapped to [`McpError::ConnectionFailed`] — from a
    /// transport/route failure (404/405 → [`McpError::TransportError`]), so a
    /// log can say *why* a connection was lost without emitting the error's
    /// detail string, which may carry URLs, headers, or credentials.
    pub fn kind(&self) -> &'static str {
        match self {
            McpError::ConnectionFailed(_) => "connection_failed",
            McpError::ProtocolError(_) => "protocol_error",
            McpError::TransportError(_) => "transport_error",
            McpError::CapabilityDenied(_) => "capability_denied",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::McpError;

    #[test]
    fn kind_is_the_variant_discriminant_not_the_detail() {
        // An expired credential (401/403) surfaces as ConnectionFailed, so its
        // kind must be distinguishable from a transport/route failure — and the
        // detail string (which may hold a token) must never appear in `kind`.
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
