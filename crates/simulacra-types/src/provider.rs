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

/// Incremental provider events emitted while a streaming response is assembled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderStreamEvent {
    /// Assistant-visible text delta.
    TextDelta { text: String },
    /// Provider started an extended thinking block.
    ThinkingStart,
    /// Provider emitted an extended thinking delta.
    ThinkingDelta { text: String },
    /// Provider ended the current extended thinking block.
    ThinkingEnd,
}

/// Non-blocking sink for provider streaming events.
pub trait ProviderStreamSink: Send + Sync + 'static {
    fn emit(&self, event: ProviderStreamEvent);
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

    fn as_streaming(&self) -> Option<&dyn StreamingProvider> {
        None
    }
}

/// Optional streaming companion contract for providers that can emit deltas.
///
/// Implementations must still return one final assembled `ProviderResponse`.
pub trait StreamingProvider: Provider {
    fn chat_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        budget: &'a mut ResourceBudget,
        sink: &'a dyn ProviderStreamSink,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    >;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NonStreamingDummyProvider;

    impl Provider for NonStreamingDummyProvider {
        fn chat<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a [ToolDefinition],
            _budget: &'a mut ResourceBudget,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Err(ProviderError::Other("not called".into())) })
        }
    }

    struct StreamingDummyProvider;

    impl Provider for StreamingDummyProvider {
        fn chat<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a [ToolDefinition],
            _budget: &'a mut ResourceBudget,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Err(ProviderError::Other("not called".into())) })
        }

        fn as_streaming(&self) -> Option<&dyn StreamingProvider> {
            Some(self)
        }
    }

    impl StreamingProvider for StreamingDummyProvider {
        fn chat_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a [ToolDefinition],
            _budget: &'a mut ResourceBudget,
            _sink: &'a dyn ProviderStreamSink,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Err(ProviderError::Other("not called".into())) })
        }
    }

    #[test]
    fn streaming_provider_contract_is_object_safe_and_optional() {
        let provider: Box<dyn Provider> = Box::new(NonStreamingDummyProvider);
        assert!(provider.as_streaming().is_none());

        let streaming_provider: Box<dyn Provider> = Box::new(StreamingDummyProvider);
        let streaming: &dyn StreamingProvider = streaming_provider
            .as_streaming()
            .expect("streaming providers expose the companion trait");
        let _object_safe: &dyn Provider = streaming;
    }
}
