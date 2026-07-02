use super::*;

pub type ChildCellConfigurator = Arc<dyn Fn(&mut simulacra_sandbox::AgentCell) + Send + Sync>;

pub type ChildToolRegistrar = Arc<
    dyn Fn(
            &mut simulacra_tool::ToolRegistry,
            Arc<simulacra_sandbox::AgentCell>,
        ) -> Result<(), simulacra_types::ToolError>
        + Send
        + Sync,
>;

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
