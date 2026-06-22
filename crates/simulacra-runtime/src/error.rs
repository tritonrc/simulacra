//! Runtime error types.

use simulacra_types::{BudgetExhausted, JournalError, ProviderError, ToolError};

/// Errors from runtime operations.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("session error: {0}")]
    Session(String),
    #[error("journal error: {0}")]
    Journal(#[from] JournalError),
    #[error("sandbox error: {0}")]
    Sandbox(String),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("tool error: {0}")]
    Tool(#[from] ToolError),
    #[error("budget exhausted: {0}")]
    BudgetExhausted(#[from] BudgetExhausted),
    #[error("capability violation: {0}")]
    CapabilityViolation(String),
    #[error("hook denied operation: {0}")]
    HookDenial(String),
    #[error("hook error: {0}")]
    HookError(String),
    #[error("hook killed execution: {hook}: {reason}")]
    HookKill { hook: String, reason: String },
    /// Journal append failed for a side-effecting operation.
    ///
    /// Per ARCHITECTURE.md "Journal Before Return": every side effect must
    /// have a journal entry written before the result returns. If the append
    /// fails, we abort the turn rather than continuing — otherwise replay
    /// would diverge silently.
    #[error("journal append failed for {entry_kind}: {source}")]
    JournalAppendFailed {
        entry_kind: &'static str,
        #[source]
        source: JournalError,
    },
    /// The supervisor was asked to spawn an agent without a task factory.
    /// This is a programmer error — use `AgentSupervisor::with_task_factory`
    /// (or `set_task_factory` when wired) before calling `spawn_agent`.
    #[error("spawn_agent called on a supervisor with no task factory configured")]
    SpawnMissingTask,
}

impl RuntimeError {
    /// Returns a reference to the inner `ProviderError` if this is a `Provider` variant.
    pub fn as_provider_error(&self) -> Option<&ProviderError> {
        match self {
            Self::Provider(e) => Some(e),
            _ => None,
        }
    }
}
