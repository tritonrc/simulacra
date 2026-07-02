use std::sync::{Arc, atomic::AtomicU64};
use std::time::Instant;

use rust_decimal::Decimal;
use simulacra_types::{CapabilityToken, ResourceBudget};

use crate::MemoryFs;
use crate::procfs::{ProcFs, ProcState};

use super::common::{
    FakeHookLister, FakeToolLister, make_procfs, make_procfs_child, make_procfs_unlimited_budget,
    procfs_read_str,
};

#[test]
fn procfs_agent_id_returns_the_agents_configured_id() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/agent/id"), "agent-abc123");
}

#[test]
fn procfs_agent_name_returns_the_agent_type_name() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/agent/name"), "researcher");
}

#[test]
fn procfs_agent_model_returns_the_model_string() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/agent/model"),
        "claude-sonnet-4-6"
    );
}

#[test]
fn procfs_agent_turn_returns_the_current_turn_number_as_a_string() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/agent/turn"), "3");
}

#[test]
fn procfs_agent_parent_id_returns_empty_string_for_root_agent() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/agent/parent_id"), "");
}

#[test]
fn procfs_agent_parent_id_returns_parent_id_for_child_agent() {
    let fs = make_procfs_child();
    assert_eq!(
        procfs_read_str(&fs, "/proc/agent/parent_id"),
        "parent-agent"
    );
}

// --- Budget tests -----------------------------------------------------------

#[test]
fn procfs_budget_max_tokens_returns_the_configured_token_limit() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/budget/max_tokens"), "100000");
}

#[test]
fn procfs_budget_used_tokens_returns_current_token_usage() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/budget/used_tokens"), "4521");
}

#[test]
fn procfs_budget_remaining_tokens_returns_max_minus_used() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/budget/remaining_tokens"),
        "95479"
    );
}

#[test]
fn procfs_budget_remaining_tokens_returns_zero_when_max_tokens_is_zero_unlimited() {
    let fs = make_procfs_unlimited_budget();
    assert_eq!(
        procfs_read_str(&fs, "/proc/budget/remaining_tokens"),
        "0",
        "unlimited budget (max=0) should report remaining_tokens as 0"
    );
}

#[test]
fn procfs_budget_max_turns_returns_the_configured_turn_limit() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/budget/max_turns"), "10");
}

#[test]
fn procfs_budget_remaining_turns_returns_max_minus_used() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/budget/remaining_turns"), "7");
}

#[test]
fn procfs_budget_used_cost_returns_cost_with_two_decimal_places() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/budget/used_cost"), "0.12");
}

#[test]
fn procfs_budget_values_are_dynamic_at_read_time() {
    // Mutate the shared budget between reads and confirm ProcFs reflects it.
    let budget_arc = {
        let b = ResourceBudget::new(100_000, 10, Decimal::ZERO, 0);
        Arc::new(std::sync::Mutex::new(b))
    };
    let state = Arc::new(ProcState {
        agent_id: "agent-dynamic".to_string(),
        agent_name: "default".to_string(),
        model: "model".to_string(),
        parent_id: None,
        budget: Arc::clone(&budget_arc),
        capabilities: CapabilityToken::default(),
        tools: FakeToolLister::default_tools(),
        session_id: "s".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: FakeHookLister::empty(),
        turn: Arc::new(AtomicU64::new(0)),
    });
    let fs = ProcFs::new(MemoryFs::new(), state);

    let before = procfs_read_str(&fs, "/proc/budget/used_tokens");
    budget_arc.lock().unwrap().used_tokens = 999;
    let after = procfs_read_str(&fs, "/proc/budget/used_tokens");

    assert_eq!(before, "0");
    assert_eq!(after, "999");
}
