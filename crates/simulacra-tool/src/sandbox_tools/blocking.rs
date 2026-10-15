use std::sync::Arc;

use simulacra_sandbox::AgentCell;
use simulacra_types::ToolError;

/// Run a synchronous `AgentCell` call on a blocking thread. The VFS behind
/// the cell may be remote, and a call made on the async worker would stall
/// every other task on it.
///
/// The closure runs inside the caller's current span, so sandbox spans stay
/// children of the tool-call span. Dropping the returned future does not stop
/// the closure: a write or patch may still commit after the caller gave up.
/// Synchronous calls were never interruptible, and `apply_vfs_mutations` is
/// atomic, so the result is a complete change or none.
pub(super) async fn on_blocking_thread<T, F>(cell: &Arc<AgentCell>, f: F) -> Result<T, ToolError>
where
    T: Send + 'static,
    F: FnOnce(&AgentCell) -> T + Send + 'static,
{
    let cell = Arc::clone(cell);
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || span.in_scope(|| f(&cell)))
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("file tool worker failed: {e}")))
}
