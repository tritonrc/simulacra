//! Shell HTTP proxy trait for intercepting network commands (curl, wget).
//!
//! Instead of allowing raw network access from shell builtins, HTTP requests
//! are routed through this proxy trait, which enforces capability checks,
//! budget limits, and audit logging.

use std::fmt;

/// Response from a shell HTTP proxy call.
#[derive(Debug, Clone)]
pub struct ShellHttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub url: String,
}

/// Error from a shell HTTP proxy call.
#[derive(Debug)]
pub enum ShellHttpError {
    CapabilityDenied(String),
    BudgetExhausted(String),
    NetworkError(String),
    Timeout,
}

impl fmt::Display for ShellHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShellHttpError::CapabilityDenied(msg) => write!(f, "capability denied: {msg}"),
            ShellHttpError::BudgetExhausted(msg) => write!(f, "budget exhausted: {msg}"),
            ShellHttpError::NetworkError(msg) => write!(f, "network error: {msg}"),
            ShellHttpError::Timeout => write!(f, "request timed out"),
        }
    }
}

impl std::error::Error for ShellHttpError {}

/// Trait for proxying HTTP requests from shell builtins (curl, wget).
///
/// Implementations enforce capability checks, budget tracking, and audit
/// logging before forwarding the request to the actual HTTP client.
pub trait ShellHttpProxy: Send + Sync {
    fn execute(
        &self,
        url: &str,
        method: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout_ms: Option<u64>,
    ) -> Result<ShellHttpResponse, ShellHttpError>;
}
