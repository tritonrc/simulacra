//! Errors for the memory subsystem.

use simulacra_types::MemoryPathError;

/// All errors from `MemoryStore`, `VectorIndex`, `Embedder`, `Chunker`.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid path: {0}")]
    InvalidPath(#[from] MemoryPathError),

    #[error(
        "embedder mismatch — stored: {stored}, configured: {configured}, requires_wipe: {requires_wipe}"
    )]
    EmbedderMismatch {
        stored: String,
        configured: String,
        requires_wipe: bool,
    },

    #[error(
        "embedder dimension mismatch — stored: {stored}, configured: {configured}; only wipe_and_rebuild can change dimension"
    )]
    EmbedderDimensionMismatch { stored: usize, configured: usize },

    #[error("embedding failed: {0}")]
    EmbeddingFailed(String),

    #[error("vector is not unit-normalized: norm = {0}")]
    NotUnitVector(f32),

    #[error("vector dimension mismatch: expected {expected}, got {got}")]
    VectorDimMismatch { expected: usize, got: usize },

    #[error("dedup quota exceeded: {message}")]
    DedupQuotaExceeded { message: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("internal error: {0}")]
    Internal(String),

    /// S038: `BackgroundEmbedder::shutdown` drain exceeded the configured
    /// timeout while waiting for at least one worker to finish.
    #[error("background embedder shutdown timed out waiting for workers to drain")]
    ShutdownTimeout,

    /// S038: a per-tenant background embedder worker panicked during
    /// shutdown. The shutdown path continues draining the rest and
    /// surfaces this as the overall shutdown error.
    #[error("background embedder worker panicked for tenant: {tenant}")]
    WorkerPanic { tenant: simulacra_types::TenantId },
}
