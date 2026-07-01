//! The core ReAct agent loop.
//!
//! Composites: provider + tool registry + context strategy + journal + budget.
//! Policy (budget, compaction, telemetry) is injected, not hardcoded.
//! ExitReason enum controls termination.

mod construct;
mod journal;
mod meters;
mod replay_helpers;
mod run;
mod tool_execution;
mod turn;
mod types;

#[cfg(test)]
mod tests;

pub use types::{AgentLoopConfig, AgentLoopOutput, TurnResult};

use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex};

use std::sync::atomic::Ordering;
use std::time::Instant;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use rust_decimal::Decimal;
use simulacra_hooks::pipeline::HookPipeline;
use simulacra_tool::ToolRegistry;
use simulacra_types::{
    ActivityEvent, AgentId, CapabilityToken, CheckpointData, Clock, ContextStrategy, ExitReason,
    JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, JournalStorage, Message, Provider,
    ResourceBudget, Role, SystemClock, TokenUsage, VfsSnapshot, VirtualFs,
};

use crate::RuntimeError;
use crate::activity_sink::{ActivitySink, NoopActivitySink};
use crate::replay::JournalReplayIterator;
use meters::RuntimeMeters;
use replay_helpers::{
    describe_replay_entry, entry_kind_name, replay_entries_match, replay_llm_response,
    replay_tool_result,
};
use tool_execution::execute_tool_live;

/// The core ReAct agent loop.
///
/// Runs: receive task -> [LLM -> tool calls -> journal -> repeat] -> exit.
/// Supports replay: when given a replay journal, replays recorded results
/// until the frontier, then switches to live execution.
pub struct AgentLoop {
    config: AgentLoopConfig,
    provider: Box<dyn Provider>,
    tools: ToolRegistry,
    context_strategy: Box<dyn ContextStrategy>,
    journal: Arc<dyn JournalStorage>,
    budget: ResourceBudget,
    budget_mirror: Option<Arc<Mutex<ResourceBudget>>>,
    turn_mirror: Option<Arc<AtomicU64>>,
    clock: Box<dyn Clock>,
    replay: Option<JournalReplayIterator>,
    /// Governance hook pipeline for LLM call interception (S026).
    pipeline: Option<Arc<HookPipeline>>,
    /// Activity sink for real-time event emission (S019).
    /// If None at construction, a `NoopActivitySink` is used.
    sink: Arc<dyn ActivitySink>,
    /// Count of journal write failures since last drain.
    /// Surfaced to the caller so the user sees a warning instead of silent data loss.
    journal_write_failures: AtomicU32,
    /// Optional VFS handle used to restore `vfs_snapshot` from a `CheckpointData`
    /// during replay-from-checkpoint. When `None`, VFS state is not restored
    /// (tests and some in-process callers may legitimately skip this).
    vfs: Option<Arc<dyn VirtualFs>>,
}
