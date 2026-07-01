use simulacra_types::ExitReason;

/// Convert an `ExitReason` to a snake_case string per spec.
pub(crate) fn exit_reason_to_snake_case(reason: &ExitReason) -> String {
    match reason {
        ExitReason::Complete => "completed".into(),
        ExitReason::MaxTurns => "max_turns".into(),
        ExitReason::BudgetExhausted => "budget_exhausted".into(),
        ExitReason::GuardrailTripped(s) => format!("guardrail_tripped:{s}"),
        ExitReason::AwaitingApproval => "awaiting_approval".into(),
        ExitReason::Cancelled => "cancelled".into(),
        ExitReason::PolicyKill { hook, reason } => {
            format!("policy_kill:{hook}:{reason}")
        }
        ExitReason::Error(s) => format!("error:{s}"),
    }
}
