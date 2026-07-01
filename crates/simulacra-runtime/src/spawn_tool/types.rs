use super::*;

// ---------------------------------------------------------------------------
// ProviderKind
// ---------------------------------------------------------------------------

/// Which LLM provider backend to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    OpenAI,
    Ollama,
}

// ---------------------------------------------------------------------------
// NoopContextStrategy
// ---------------------------------------------------------------------------

/// A context strategy that performs no compaction — returns messages as-is.
pub struct NoopContextStrategy;

impl ContextStrategy for NoopContextStrategy {
    fn compact(&self, messages: &[Message], _token_limit: u64) -> Vec<Message> {
        messages.to_vec()
    }
}
