use thiserror::Error;

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("validation: {0}")]
    Validation(String),

    #[error("read-only repository: {0}")]
    ReadOnly(String),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("join: {0}")]
    Join(#[from] tokio::task::JoinError),
}
