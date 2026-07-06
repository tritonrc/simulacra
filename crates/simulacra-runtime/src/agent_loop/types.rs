use super::*;

/// Configuration for the agent loop.
#[derive(Debug, Clone)]
pub struct AgentLoopConfig {
    pub agent_id: AgentId,
    pub system_prompt: String,
    pub model: String,
    pub max_turns: u32,
    pub capability: CapabilityToken,
}

/// Output from the agent loop.
#[derive(Debug, Clone)]
pub struct AgentLoopOutput {
    pub exit_reason: ExitReason,
    pub messages: Vec<Message>,
    pub token_usage: TokenUsage,
    /// Optional structured tool-use count reported by a non-native child
    /// runtime, such as ACP.
    pub reported_tool_uses: Option<u64>,
    /// Total turns consumed by this agent loop invocation.
    pub used_turns: u32,
    /// Total cost consumed by this agent loop invocation.
    pub used_cost: Decimal,
}

/// Immutable provider-call snapshot for a single model step.
#[derive(Debug, Clone)]
pub struct StepContext {
    messages: Vec<Message>,
    tool_definitions: Vec<simulacra_types::ToolDefinition>,
}

impl StepContext {
    pub fn new(
        messages: Vec<Message>,
        tool_definitions: Vec<simulacra_types::ToolDefinition>,
    ) -> Self {
        Self {
            messages,
            tool_definitions,
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn tool_definitions(&self) -> &[simulacra_types::ToolDefinition] {
        &self.tool_definitions
    }
}

/// Stable per-turn context shared by runtime subsystems.
#[derive(Debug, Clone)]
pub struct TurnContext {
    agent_id: AgentId,
    model: String,
    capability: CapabilityToken,
    cancellation: Option<crate::CancellationToken>,
}

impl TurnContext {
    pub fn new(
        agent_id: AgentId,
        model: String,
        capability: CapabilityToken,
        cancellation: Option<crate::CancellationToken>,
    ) -> Self {
        Self {
            agent_id,
            model,
            capability,
            cancellation,
        }
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn capability(&self) -> &CapabilityToken {
        &self.capability
    }

    pub fn cancellation(&self) -> Option<&crate::CancellationToken> {
        self.cancellation.as_ref()
    }
}

/// Mutable state accumulated while a turn is active.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnState {
    pub tool_call_count: u64,
    pub cancelled: bool,
}

/// Runtime handle for one active turn.
#[derive(Debug, Clone)]
pub struct ActiveTurn {
    context: TurnContext,
    state: Arc<Mutex<TurnState>>,
}

impl ActiveTurn {
    pub fn new(context: TurnContext) -> Self {
        Self {
            context,
            state: Arc::new(Mutex::new(TurnState::default())),
        }
    }

    pub fn context(&self) -> &TurnContext {
        &self.context
    }

    pub fn state(&self) -> TurnState {
        self.state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    pub fn record_tool_call(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.tool_call_count = state.tool_call_count.saturating_add(1);
    }

    pub fn mark_cancelled(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled = true;
    }
}

/// Result of a single turn in the agent loop.
#[derive(Debug)]
pub enum TurnResult {
    /// Model produced a final text response (no tool calls).
    Complete(Message),
    /// Model requested tool calls. Contains the assistant message with tool_calls
    /// and the tool results that were dispatched.
    ToolCallsProcessed {
        assistant_message: Message,
        tool_results: Vec<Message>,
    },
    /// Budget exhausted before the turn could run.
    BudgetExhausted,
    /// Runtime cancellation was observed before starting more work.
    Cancelled,
}
