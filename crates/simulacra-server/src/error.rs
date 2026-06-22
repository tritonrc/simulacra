//! Error types for simulacra-server.

use thiserror::Error;

/// Top-level server error.
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("bind error on {addr}: {source}")]
    Bind {
        addr: String,
        source: std::io::Error,
    },
}

/// API-level error returned in response envelopes.
#[derive(Debug, Clone, Error)]
#[error("{code}: {message}")]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn invalid_message(msg: impl Into<String>) -> Self {
        Self::new("invalid_message", msg)
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new("unauthorized", msg)
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new("forbidden", msg)
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new("not_found", msg)
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new("bad_request", msg)
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new("internal_error", msg)
    }
}
