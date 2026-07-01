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
