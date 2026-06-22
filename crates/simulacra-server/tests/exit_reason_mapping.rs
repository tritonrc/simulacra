//! Tests for ExitReason → TaskState mapping (S034 assertions: agent completion).

use simulacra_server::{TaskState, map_exit_reason};
use simulacra_types::ExitReason;

#[test]
fn complete_maps_to_completed_with_no_reason() {
    let (state, reason) = map_exit_reason(&ExitReason::Complete);
    assert_eq!(state, TaskState::Completed);
    assert_eq!(reason, None);
}

#[test]
fn max_turns_maps_to_completed_with_max_turns_reason() {
    let (state, reason) = map_exit_reason(&ExitReason::MaxTurns);
    assert_eq!(state, TaskState::Completed);
    assert_eq!(reason.as_deref(), Some("max_turns"));
}

#[test]
fn budget_exhausted_maps_to_killed_with_budget_exhausted_reason() {
    let (state, reason) = map_exit_reason(&ExitReason::BudgetExhausted);
    assert_eq!(state, TaskState::Killed);
    assert_eq!(reason.as_deref(), Some("budget_exhausted"));
}

#[test]
fn cancelled_maps_to_cancelled_with_no_reason() {
    let (state, reason) = map_exit_reason(&ExitReason::Cancelled);
    assert_eq!(state, TaskState::Cancelled);
    assert_eq!(reason, None);
}

#[test]
fn guardrail_tripped_maps_to_killed_with_the_guardrail_message_in_reason() {
    let (state, reason) =
        map_exit_reason(&ExitReason::GuardrailTripped("PII detected".to_string()));
    assert_eq!(state, TaskState::Killed);
    let r = reason.expect("guardrail failures should include a reason");
    assert!(r.contains("PII detected"));
    assert!(r.contains("guardrail"));
}

#[test]
fn policy_kill_maps_to_killed_with_hook_and_reason_details() {
    let (state, reason) = map_exit_reason(&ExitReason::PolicyKill {
        hook: "governance.pre_tool".to_string(),
        reason: "denied network egress".to_string(),
    });
    assert_eq!(state, TaskState::Killed);
    let r = reason.expect("policy kills should include a reason");
    assert!(r.contains("governance.pre_tool"));
    assert!(r.contains("denied network egress"));
}

#[test]
fn error_maps_to_failed_with_the_runtime_error_message() {
    let (state, reason) = map_exit_reason(&ExitReason::Error("provider timeout".to_string()));
    assert_eq!(state, TaskState::Failed);
    assert_eq!(reason.as_deref(), Some("provider timeout"));
}

#[test]
fn awaiting_approval_maps_to_waiting_approval_and_is_non_terminal() {
    let (state, reason) = map_exit_reason(&ExitReason::AwaitingApproval);
    assert_eq!(state, TaskState::WaitingApproval);
    assert_eq!(reason, None);
    // WaitingApproval is NOT terminal.
    assert!(!state.is_terminal());
}
