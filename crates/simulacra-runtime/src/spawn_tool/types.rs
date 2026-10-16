use std::future::Future;
use std::pin::Pin;

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

/// What a host gives one native child: the filesystem it sees and how its
/// `file_read` presents. Asked once per spawn, inside the spawned task, so the
/// host can open per-child state with the child's id.
pub struct ChildFsEnvironment {
    pub vfs: Arc<dyn VirtualFs>,
    pub file_presenter: Option<Arc<dyn simulacra_tool::FilePresenter>>,
}

pub type ChildEnvironmentProvider = Arc<
    dyn Fn(
            &SpawnConfig,
        )
            -> Pin<Box<dyn Future<Output = Result<ChildFsEnvironment, RuntimeError>> + Send>>
        + Send
        + Sync,
>;

/// Creates a fresh provider for each spawned child agent.
///
/// The factory receives the resolved provider kind and model for the child.
/// Production callers leave this unset and use the runtime's normal provider
/// adapter; headless harnesses can inject scripted providers for offline child
/// orchestration tests.
pub type ChildProviderFactory =
    Arc<dyn Fn(&ProviderKind, &str) -> Result<Box<dyn Provider>, RuntimeError> + Send + Sync>;

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
