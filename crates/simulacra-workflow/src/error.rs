use simulacra_types::VfsError;

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("invalid workflow script path `{path}`: {reason}")]
    InvalidScriptPath { path: String, reason: String },

    #[error("invalid workflow script metadata: {0}")]
    InvalidMetadata(String),

    #[error("invalid workflow script: {0}")]
    InvalidScript(String),

    #[error("workflow run `{run_id}` was not found")]
    RunNotFound { run_id: String },

    #[error("workflow worker failed for call `{key}`: {message}")]
    WorkerFailed { key: String, message: String },

    #[error("workflow run was cancelled")]
    Cancelled,

    #[error("virtual filesystem error: {0}")]
    Vfs(#[from] VfsError),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("internal workflow error: {0}")]
    Internal(String),
}
