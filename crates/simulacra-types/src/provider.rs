use crate::{Message, ResourceBudget, TokenUsage, ToolDefinition};
use serde::{Deserialize, Serialize};

/// Response from an LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderResponse {
    pub message: Message,
    pub token_usage: TokenUsage,
    pub finish_reason: FinishReason,
    pub provider_response_id: Option<String>,
    pub model: String,
}

/// Why the provider stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

/// Errors from provider operations.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("rate limited: retry after {retry_after_ms:?}ms")]
    RateLimit { retry_after_ms: Option<u64> },
    #[error("authentication error: {0}")]
    AuthError(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("server error: {0}")]
    ServerError(String),
    #[error("overloaded: {0}")]
    Overloaded(String),
    #[error("budget exhausted: {0}")]
    BudgetExhausted(#[from] crate::BudgetExhausted),
    #[error("other: {0}")]
    Other(String),
}

impl ProviderError {
    /// Classify an HTTP status code and message into a typed error.
    pub fn classify(status: u16, message: impl Into<String>) -> Self {
        let msg = message.into();
        match status {
            401 | 403 => Self::AuthError(msg),
            400 => Self::BadRequest(msg),
            429 => Self::RateLimit {
                retry_after_ms: None,
            },
            529 => Self::Overloaded(msg),
            500..=599 => Self::ServerError(msg),
            _ => Self::Other(format!("HTTP {status}: {msg}")),
        }
    }

    /// Whether this error is transient and the request should be retried.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimit { .. } | Self::ServerError(_) | Self::Overloaded(_)
        )
    }
}

/// Why the agent loop terminated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    /// Model produced final text (no tool calls).
    Complete,
    /// Turn limit reached.
    MaxTurns,
    /// Budget exhausted (tokens, cost, or sub-agents).
    BudgetExhausted,
    /// Guardrail halted execution.
    GuardrailTripped(String),
    /// Awaiting human approval for a tool call.
    AwaitingApproval,
    /// Cancelled by supervisor or user.
    Cancelled,
    /// Governance hook killed execution (S026).
    PolicyKill { hook: String, reason: String },
    /// Unrecoverable error.
    Error(String),
}

/// LLM provider trait. Object-safe.
pub trait Provider: Send + Sync + 'static {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    >;
}
