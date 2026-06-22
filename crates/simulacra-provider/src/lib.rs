//! Simulacra provider crate.
//!
//! Adapters that implement the `Provider` trait from simulacra-types
//! for concrete LLM backends.

pub use simulacra_types::{
    FinishReason, Message, Provider, ProviderError, ProviderResponse, ResourceBudget, TokenUsage,
    ToolDefinition,
};

#[cfg(feature = "anthropic")]
mod anthropic;

#[cfg(feature = "anthropic")]
pub use anthropic::AnthropicProvider;

#[cfg(feature = "openai")]
mod openai;

#[cfg(feature = "openai")]
pub use openai::OpenAiProvider;
