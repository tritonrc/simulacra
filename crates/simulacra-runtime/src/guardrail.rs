//! Guardrail traits for input/output message filtering.

use simulacra_types::Message;

/// Decision from a guardrail check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardrailDecision {
    /// Allow the message to proceed.
    Pass,
    /// Hard stop — the message must not proceed.
    Tripwire(String),
    /// Soft warning — the message can proceed but should be flagged.
    Warn(String),
}

/// Guardrail applied to incoming messages before they reach the agent.
pub trait InputGuardrail: Send + Sync + 'static {
    fn check(&self, message: &Message) -> GuardrailDecision;
}

/// Guardrail applied to outgoing messages before they are returned to the user.
pub trait OutputGuardrail: Send + Sync + 'static {
    fn check(&self, message: &Message) -> GuardrailDecision;
}
