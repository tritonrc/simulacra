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
    #[error(
        "ACP backend requested for agent_type '{agent_type}' with acp_profile '{acp_profile}', but no ACP child runtime was injected; configure an AcpChildRuntime before spawning ACP children"
    )]
    AcpChildRuntimeMissing {
        agent_type: String,
        acp_profile: String,
    },
    /// The workspace backing a running task disappeared.
    ///
    /// Use this instead of `Session` when a consumer needs to classify "the
    /// workspace is gone" by type rather than by matching free-text error
    /// messages. `cause` distinguishes an abrupt transport-level
    /// disappearance from a clean close initiated by the workspace's own
    /// side, and from a runtime-initiated teardown.
    #[error("workspace lost: {cause}")]
    WorkspaceLost { cause: WorkspaceLostCause },
}

/// Why a workspace was lost.
///
/// Kept generic and embedding-agnostic: Simulacra only knows the shape of
/// the failure it observed, not the embedding-specific reason a workspace is
/// actually gone (e.g. a killed process or a network partition).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceLostCause {
    /// The workspace vanished abruptly: a transport EOF or error was
    /// observed with no clean shutdown, e.g. the workspace was killed,
    /// crashed, or became unreachable over the network.
    Gone,
    /// The workspace ended cleanly and deliberately on its own side.
    Closed,
    /// The runtime itself tore the workspace down, e.g. reclaiming an idle
    /// workspace.
    Reaped,
}

impl std::fmt::Display for WorkspaceLostCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Gone => "gone",
            Self::Closed => "closed",
            Self::Reaped => "reaped",
        };
        write!(f, "{s}")
    }
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

#[cfg(test)]
mod tests {
    use super::{RuntimeError, WorkspaceLostCause};

    #[test]
    fn workspace_lost_is_distinct_from_free_text_session_error() {
        let error = RuntimeError::WorkspaceLost {
            cause: WorkspaceLostCause::Gone,
        };

        match error {
            RuntimeError::WorkspaceLost {
                cause: WorkspaceLostCause::Gone,
            } => {}
            RuntimeError::Session(message) => {
                panic!("workspace loss must not be classified by Session text: {message}");
            }
            other => panic!("expected WorkspaceLost error, got {other:?}"),
        }
    }

    #[test]
    fn workspace_lost_display_includes_typed_cause() {
        assert_eq!(
            RuntimeError::WorkspaceLost {
                cause: WorkspaceLostCause::Gone
            }
            .to_string(),
            "workspace lost: gone"
        );
        assert_eq!(
            RuntimeError::WorkspaceLost {
                cause: WorkspaceLostCause::Closed
            }
            .to_string(),
            "workspace lost: closed"
        );
        assert_eq!(
            RuntimeError::WorkspaceLost {
                cause: WorkspaceLostCause::Reaped
            }
            .to_string(),
            "workspace lost: reaped"
        );
    }

    #[test]
    fn workspace_lost_cause_distinguishes_abrupt_disappearance_clean_close_and_reap() {
        assert_ne!(WorkspaceLostCause::Gone, WorkspaceLostCause::Closed);
        assert_ne!(WorkspaceLostCause::Gone, WorkspaceLostCause::Reaped);
        assert_ne!(WorkspaceLostCause::Closed, WorkspaceLostCause::Reaped);
    }
}
