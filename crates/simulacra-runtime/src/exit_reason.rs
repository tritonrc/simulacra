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
        ExitReason::PolicyKill { .. } => "policy_kill".into(),
        // S018 §5a: a terminal child summary's `exit_reason` is one of
        // completed/budget_exhausted/max_turns/cancelled/failed — not an
        // open vocabulary. A refusal is an accepted-child runtime failure,
        // so it maps to "failed" rather than introducing a sixth value.
        ExitReason::Refusal => "failed".into(),
        // Keep the machine-readable terminal category stable. The detailed
        // error remains available in the typed output/message and must not be
        // folded into a status-like wire value.
        ExitReason::Error(_) => "error".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S018 §5a: the child-summary `exit_reason` wire field is a closed
    /// vocabulary. A refusal must not leak out as a sixth value.
    #[test]
    fn refusal_maps_to_failed_per_s018_5a() {
        assert_eq!(exit_reason_to_snake_case(&ExitReason::Refusal), "failed");
    }
}
