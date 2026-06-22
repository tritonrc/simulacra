use thiserror::Error;

/// Errors that can occur during hook execution.
#[derive(Debug, Error)]
pub enum HookError {
    /// The hook denied the operation.
    #[error("hook '{hook}' denied: {reason}")]
    Denied { hook: String, reason: String },

    /// The hook killed the agent.
    #[error("hook '{hook}' killed agent: {reason}")]
    Killed { hook: String, reason: String },

    /// The hook timed out.
    #[error("hook '{hook}' timed out after {timeout_ms}ms")]
    Timeout { hook: String, timeout_ms: u64 },

    /// The hook script failed to execute.
    #[error("hook '{hook}' execution error: {message}")]
    ExecutionError { hook: String, message: String },
}
