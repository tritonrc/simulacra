//! Artifact storage trait and types for durable task output.

/// Metadata for a single artifact.
#[derive(Debug, Clone)]
pub struct ArtifactEntry {
    /// Relative path within the task's artifact namespace (e.g. "summary.md", "reports/q1.csv").
    pub path: String,
    /// Size in bytes.
    pub size: u64,
}

/// Errors from artifact store operations.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("artifact not found: {0}")]
    NotFound(String),
    #[error("invalid artifact path: {0}")]
    InvalidPath(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Durable artifact storage, keyed by (task_id, path).
///
/// Implementations persist artifacts immediately on `put` — callers must not
/// assume data is held in memory. The `tenant` parameter on `put` is used for
/// directory isolation and retention scoping.
pub trait ArtifactStore: Send + Sync + 'static {
    /// Persist an artifact. Overwrites if path exists. Must be atomic:
    /// readers see either the old content or the new content, never partial.
    fn put(
        &self,
        task_id: &str,
        tenant: &str,
        path: &str,
        data: &[u8],
    ) -> Result<(), ArtifactError>;

    /// Retrieve artifact bytes. Returns `ArtifactError::NotFound` if missing.
    /// The `tenant` parameter scopes the lookup to prevent cross-tenant reads.
    fn get(&self, tenant: &str, task_id: &str, path: &str) -> Result<Vec<u8>, ArtifactError>;

    /// List all artifacts for a task. Recursive. Returns relative paths + sizes.
    /// The `tenant` parameter scopes the listing to prevent cross-tenant reads.
    fn list(&self, tenant: &str, task_id: &str) -> Result<Vec<ArtifactEntry>, ArtifactError>;

    /// Delete all artifacts for a task (cleanup/retention).
    /// The `tenant` parameter scopes the deletion to prevent cross-tenant operations.
    fn delete_task(&self, tenant: &str, task_id: &str) -> Result<(), ArtifactError>;
}
