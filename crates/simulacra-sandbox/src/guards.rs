//! Shared guard helpers for capability checks, budget checks, and journal writes.
//!
//! These helpers encapsulate the repeated capability-check + budget-check + journal-write
//! pattern used across sandbox operations.

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use simulacra_types::{
    AgentId, BudgetExhausted, CapabilityDenied, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalStorage, ResourceBudget,
};
use std::sync::{Arc, Mutex};

use crate::SandboxError;

// ── OTel meters ──────────────────────────────────────────────────

/// Lazily-initialized OTel meter instruments for sandbox guards.
/// Created on first use so they pick up the global MeterProvider
/// (which may not be set at construction time).
struct GuardMeters {
    capability_denials: Counter<u64>,
    budget_exhaustions: Counter<u64>,
}

impl GuardMeters {
    fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<GuardMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-sandbox");
            GuardMeters {
                capability_denials: meter
                    .u64_counter("simulacra.capability.denials")
                    .with_description("Total capability denials")
                    .build(),
                budget_exhaustions: meter
                    .u64_counter("simulacra.budget.exhaustions")
                    .with_description("Total budget exhaustions")
                    .build(),
            }
        })
    }
}

/// Check a capability, and on denial: journal, log, emit OTel counter, and return error.
///
/// `check` is a closure that performs the actual capability check (e.g.,
/// `capability.check_shell()`). `operation` is used for journal and tracing context.
pub(crate) fn check_and_journal_capability<F>(
    check: F,
    operation: &str,
    capability_name: &str,
    journal: &Arc<dyn JournalStorage>,
    agent_id: &AgentId,
) -> Result<(), SandboxError>
where
    F: FnOnce() -> Result<(), CapabilityDenied>,
{
    if let Err(denied) = check() {
        journal_denial(journal, agent_id, operation, &denied);
        tracing::warn!(
            simulacra.capability.operation = %operation,
            simulacra.capability.reason = %denied.reason,
            "capability denied"
        );
        tracing::info!(
            simulacra.capability.denials = 1u64,
            operation = %capability_name,
            "capability denial counter"
        );
        GuardMeters::get().capability_denials.add(
            1,
            &[
                KeyValue::new(
                    "simulacra.capability.operation",
                    capability_name.to_string(),
                ),
                KeyValue::new("simulacra.agent.id", agent_id.0.clone()),
            ],
        );
        return Err(SandboxError::CapabilityDenied(denied));
    }
    Ok(())
}

/// Check turns budget and return an error if exhausted.
pub(crate) fn check_turns_budget(
    budget: &Arc<Mutex<ResourceBudget>>,
    journal: &Arc<dyn JournalStorage>,
    agent_id: &AgentId,
) -> Result<(), SandboxError> {
    let b = budget
        .lock()
        .map_err(|e| SandboxError::Internal(format!("budget mutex poisoned: {e}")))?;
    if b.max_turns > 0 && b.used_turns >= b.max_turns {
        let exhausted = BudgetExhausted {
            resource: "turns".into(),
            used: b.used_turns.to_string(),
            limit: b.max_turns.to_string(),
        };
        journal_budget_exhaustion(journal, agent_id, &exhausted);
        tracing::warn!(
            simulacra.budget.resource = "turns",
            simulacra.budget.used = %b.used_turns,
            simulacra.budget.limit = %b.max_turns,
            "budget exhausted"
        );
        GuardMeters::get().budget_exhaustions.add(
            1,
            &[
                KeyValue::new("simulacra.budget.resource", "turns"),
                KeyValue::new("simulacra.agent.id", agent_id.0.clone()),
            ],
        );
        return Err(SandboxError::BudgetExhausted(exhausted));
    }
    Ok(())
}

/// Atomically check and reserve one turn before an operation executes.
pub(crate) fn reserve_turn(
    budget: &Arc<Mutex<ResourceBudget>>,
    journal: &Arc<dyn JournalStorage>,
    agent_id: &AgentId,
) -> Result<(), SandboxError> {
    let mut b = budget
        .lock()
        .map_err(|e| SandboxError::Internal(format!("budget mutex poisoned: {e}")))?;
    if b.max_turns > 0 && b.used_turns >= b.max_turns {
        let exhausted = BudgetExhausted {
            resource: "turns".into(),
            used: b.used_turns.to_string(),
            limit: b.max_turns.to_string(),
        };
        journal_budget_exhaustion(journal, agent_id, &exhausted);
        tracing::warn!(
            simulacra.budget.resource = "turns",
            simulacra.budget.used = %b.used_turns,
            simulacra.budget.limit = %b.max_turns,
            "budget exhausted"
        );
        GuardMeters::get().budget_exhaustions.add(
            1,
            &[
                KeyValue::new("simulacra.budget.resource", "turns"),
                KeyValue::new("simulacra.agent.id", agent_id.0.clone()),
            ],
        );
        return Err(SandboxError::BudgetExhausted(exhausted));
    }
    b.used_turns += 1;
    Ok(())
}

/// Atomically check global budget and reserve VFS bytes before a write executes.
pub(crate) fn reserve_vfs_bytes(
    budget: &Arc<Mutex<ResourceBudget>>,
    bytes: u64,
    journal: &Arc<dyn JournalStorage>,
    agent_id: &AgentId,
) -> Result<(), SandboxError> {
    let mut b = budget
        .lock()
        .map_err(|e| SandboxError::Internal(format!("budget mutex poisoned: {e}")))?;
    if let Err(exhausted) = b.check_budget() {
        journal_budget_exhaustion(journal, agent_id, &exhausted);
        tracing::warn!(
            simulacra.budget.resource = %exhausted.resource,
            simulacra.budget.used = %exhausted.used,
            simulacra.budget.limit = %exhausted.limit,
            "budget exhausted"
        );
        return Err(SandboxError::BudgetExhausted(exhausted));
    }

    let projected = b.used_vfs_bytes.saturating_add(bytes);
    if b.max_vfs_bytes > 0 && projected > b.max_vfs_bytes {
        let exhausted = BudgetExhausted {
            resource: "vfs_bytes".into(),
            used: projected.to_string(),
            limit: b.max_vfs_bytes.to_string(),
        };
        journal_budget_exhaustion(journal, agent_id, &exhausted);
        tracing::warn!(
            simulacra.budget.resource = "vfs_bytes",
            simulacra.budget.used = %projected,
            simulacra.budget.limit = %b.max_vfs_bytes,
            "budget exhausted"
        );
        return Err(SandboxError::BudgetExhausted(exhausted));
    }

    b.used_vfs_bytes = projected;
    Ok(())
}

/// Roll back a previous VFS byte reservation when the underlying write fails.
pub(crate) fn release_vfs_bytes(
    budget: &Arc<Mutex<ResourceBudget>>,
    bytes: u64,
) -> Result<(), SandboxError> {
    let mut b = budget
        .lock()
        .map_err(|e| SandboxError::Internal(format!("budget mutex poisoned: {e}")))?;
    b.used_vfs_bytes = b.used_vfs_bytes.saturating_sub(bytes);
    Ok(())
}

/// Write a journal entry for a capability denial.
pub(crate) fn journal_denial(
    journal: &Arc<dyn JournalStorage>,
    agent_id: &AgentId,
    operation: &str,
    denied: &CapabilityDenied,
) {
    let _ = journal.append(JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: agent_id.clone(),
        timestamp_ms: 0,
        entry: JournalEntryKind::ToolResult {
            tool_call_id: None,
            tool_name: operation.to_string(),
            content: format!(
                "capability denied: {} - {}",
                denied.operation, denied.reason
            ),
            is_error: true,
        },
    });
}

/// Write a journal entry for a budget exhaustion.
pub(crate) fn journal_budget_exhaustion(
    journal: &Arc<dyn JournalStorage>,
    agent_id: &AgentId,
    exhausted: &BudgetExhausted,
) {
    let _ = journal.append(JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: agent_id.clone(),
        timestamp_ms: 0,
        entry: JournalEntryKind::ToolResult {
            tool_call_id: None,
            tool_name: exhausted.resource.clone(),
            content: format!(
                "budget exhausted: {} - used {}, limit {}",
                exhausted.resource, exhausted.used, exhausted.limit
            ),
            is_error: true,
        },
    });
}
