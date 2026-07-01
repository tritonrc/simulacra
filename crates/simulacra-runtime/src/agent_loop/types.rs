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
#[derive(Debug)]
pub struct AgentLoopOutput {
    pub exit_reason: ExitReason,
    pub messages: Vec<Message>,
    pub token_usage: TokenUsage,
    /// Total turns consumed by this agent loop invocation.
    pub used_turns: u32,
    /// Total cost consumed by this agent loop invocation.
    pub used_cost: Decimal,
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
}
