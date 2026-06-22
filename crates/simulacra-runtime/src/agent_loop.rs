//! The core ReAct agent loop.
//!
//! Composites: provider + tool registry + context strategy + journal + budget.
//! Policy (budget, compaction, telemetry) is injected, not hardcoded.
//! ExitReason enum controls termination.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

// ── OTel meters ──────────────────────────────────────────────────

/// Lazily-initialized OTel meter instruments for the agent runtime.
/// Created on first use so they pick up the global MeterProvider
/// (which may not be set at construction time).
struct RuntimeMeters {
    turns_counter: Counter<u64>,
    budget_tokens_used: Counter<u64>,
    budget_turns_used: Counter<u64>,
    budget_exhaustions: Counter<u64>,
}

impl RuntimeMeters {
    fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<RuntimeMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-runtime");
            RuntimeMeters {
                turns_counter: meter
                    .u64_counter("simulacra.agent.turns")
                    .with_description("Agent turns consumed")
                    .build(),
                budget_tokens_used: meter
                    .u64_counter("simulacra.agent.budget.tokens_used")
                    .with_description("Agent budget tokens used")
                    .build(),
                budget_turns_used: meter
                    .u64_counter("simulacra.agent.budget.turns_used")
                    .with_description("Agent budget turns used")
                    .build(),
                budget_exhaustions: meter
                    .u64_counter("simulacra.budget.exhaustions")
                    .with_description("Total budget exhaustions")
                    .build(),
            }
        })
    }
}
use crate::activity_sink::{ActivitySink, NoopActivitySink};
use crate::replay::JournalReplayIterator;

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

impl AgentLoop {
    /// Create a new agent loop with all dependencies injected.
    ///
    /// Accepts an optional `Arc<dyn ActivitySink>` for S019 activity events.
    /// If `None`, a `NoopActivitySink` is used.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: AgentLoopConfig,
        provider: Box<dyn Provider>,
        tools: ToolRegistry,
        context_strategy: Box<dyn ContextStrategy>,
        journal: Arc<dyn JournalStorage>,
        budget: ResourceBudget,
        activity_sink: Option<Arc<dyn ActivitySink>>,
        pipeline: Option<Arc<HookPipeline>>,
    ) -> Self {
        Self {
            config,
            provider,
            tools,
            context_strategy,
            journal,
            budget,
            budget_mirror: None,
            turn_mirror: None,
            clock: Box::new(SystemClock),
            replay: None,
            pipeline,
            sink: activity_sink.unwrap_or_else(|| Arc::new(NoopActivitySink)),
            journal_write_failures: AtomicU32::new(0),
            vfs: None,
        }
    }

    /// Create a new agent loop with an injectable clock and optional replay journal.
    #[allow(clippy::too_many_arguments)]
    pub fn with_clock_and_replay(
        config: AgentLoopConfig,
        provider: Box<dyn Provider>,
        tools: ToolRegistry,
        context_strategy: Box<dyn ContextStrategy>,
        journal: Arc<dyn JournalStorage>,
        budget: ResourceBudget,
        clock: Box<dyn Clock>,
        replay_journal: Option<Vec<JournalEntry>>,
    ) -> Self {
        Self {
            config,
            provider,
            tools,
            context_strategy,
            journal,
            budget,
            budget_mirror: None,
            turn_mirror: None,
            clock,
            replay: replay_journal.map(JournalReplayIterator::new),
            pipeline: None,
            sink: Arc::new(NoopActivitySink),
            journal_write_failures: AtomicU32::new(0),
            vfs: None,
        }
    }

    /// Mirror the loop-owned budget into shared state read by `/proc`.
    pub fn set_proc_budget_mirror(
        &mut self,
        budget: Arc<Mutex<ResourceBudget>>,
        turn: Arc<AtomicU64>,
    ) {
        self.budget_mirror = Some(budget);
        self.turn_mirror = Some(turn);
        self.sync_proc_state();
    }

    fn sync_proc_state(&self) {
        if let Some(ref mirror) = self.budget_mirror {
            match mirror.lock() {
                Ok(mut budget) => {
                    *budget = self.budget.clone();
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to sync /proc budget mirror");
                }
            }
        }
        if let Some(ref turn) = self.turn_mirror {
            turn.store(self.budget.used_turns as u64, Ordering::Relaxed);
        }
    }

    /// Attach a VFS handle used to restore `vfs_snapshot` during replay-from-checkpoint.
    ///
    /// When set, `run()` will call `VirtualFs::restore` on the checkpoint's
    /// `vfs_snapshot` (if present) before the replay loop resumes. Without this,
    /// replay-from-checkpoint loses any VFS mutations captured at checkpoint time.
    pub fn set_vfs(&mut self, vfs: Arc<dyn VirtualFs>) {
        self.vfs = Some(vfs);
    }

    /// Read-only access to the current budget state.
    pub fn budget(&self) -> &ResourceBudget {
        &self.budget
    }

    /// Return the number of journal write failures since the last drain
    /// and reset the counter to zero. The caller can use this to surface
    /// a warning to the user after a turn completes.
    pub fn drain_journal_write_failures(&self) -> u32 {
        self.journal_write_failures.swap(0, Ordering::Relaxed)
    }

    /// Get tool definitions for display (e.g. in interactive /tools command).
    pub fn tool_definitions(&self) -> Vec<simulacra_types::ToolDefinition> {
        self.tools.definitions()
    }

    /// Get the system prompt for initializing conversation messages.
    pub fn system_prompt(&self) -> &str {
        &self.config.system_prompt
    }

    /// Run exactly one LLM turn: call provider, process response, dispatch tool calls, return.
    ///
    /// The caller owns the messages vec and controls the loop. This is the
    /// building block for interactive mode.
    pub async fn run_single_turn(
        &mut self,
        messages: &mut Vec<Message>,
    ) -> Result<TurnResult, RuntimeError> {
        // 1. Check budget BEFORE the operation
        if let Err(exhausted) = self.budget.check_budget() {
            RuntimeMeters::get().budget_exhaustions.add(
                1,
                &[
                    KeyValue::new("simulacra.budget.resource", exhausted.resource.clone()),
                    KeyValue::new("simulacra.agent.id", self.config.agent_id.0.clone()),
                ],
            );
            return Ok(TurnResult::BudgetExhausted);
        }

        let tool_defs = self.tools.definitions();

        // 2. Journal TurnStart — must succeed before the LLM call side effect.
        self.journal_entry(JournalEntryKind::TurnStart)?;
        self.consume_replay_entry(&JournalEntryKind::TurnStart)?;

        // 3. Compact context (0 = unlimited → use u64::MAX as sentinel)
        let remaining_tokens = if self.budget.max_tokens == 0 {
            u64::MAX
        } else {
            self.budget
                .max_tokens
                .saturating_sub(self.budget.used_tokens)
        };
        let compacted = self.context_strategy.compact(messages, remaining_tokens);

        // 4. Journal LlmRequest — must succeed before invoking the provider.
        let llm_request = JournalEntryKind::LlmRequest {
            model: self.config.model.clone(),
            message_count: compacted.len(),
        };
        self.journal_entry(llm_request.clone())?;
        self.consume_replay_entry(&llm_request)?;

        // 5. Get LLM response (with optional governance hooks)
        // BEFORE hook
        if let Some(ref pipeline) = self.pipeline {
            let before_ctx = serde_json::json!({
                "model": &self.config.model,
                "message_count": compacted.len(),
            })
            .to_string();
            match pipeline.run_before(simulacra_hooks::verdict::Operation::Llm, &before_ctx) {
                Ok((simulacra_hooks::Verdict::Continue(_), _)) => {}
                Ok((simulacra_hooks::Verdict::Deny(reason), _)) => {
                    self.journal_entry(JournalEntryKind::HookDenial {
                        hook_name: "llm:before".into(),
                        operation: "llm".into(),
                        reason: reason.clone(),
                    })?;
                    return Err(RuntimeError::HookDenial(reason));
                }
                Ok((simulacra_hooks::Verdict::Kill(_), _)) => {
                    unreachable!("Kill is returned as Err from run_before")
                }
                Err(simulacra_hooks::HookError::Killed { hook, reason }) => {
                    self.journal_entry(JournalEntryKind::HookDenial {
                        hook_name: hook.clone(),
                        operation: "llm".into(),
                        reason: reason.clone(),
                    })?;
                    return Err(RuntimeError::HookKill { hook, reason });
                }
                Err(e) => {
                    return Err(RuntimeError::HookError(e.to_string()));
                }
            }
        }

        let response = if self.has_replay_entry() {
            let kind = self.take_replay_entry()?;
            replay_llm_response(&kind)?
        } else {
            self.provider
                .chat(&compacted, &tool_defs, &mut self.budget)
                .await
                .map_err(RuntimeError::from)?
        };

        // AFTER hook
        if let Some(ref pipeline) = self.pipeline {
            let after_ctx = serde_json::json!({
                "model": &response.model,
                "content": &response.message.content,
                "tool_calls": &response.message.tool_calls,
                "usage": {
                    "input_tokens": response.token_usage.input_tokens,
                    "output_tokens": response.token_usage.output_tokens,
                },
            })
            .to_string();
            match pipeline.run_after(simulacra_hooks::verdict::Operation::Llm, &after_ctx) {
                Ok(_) => {}
                Err(simulacra_hooks::HookError::Killed { hook, reason }) => {
                    self.journal_entry(JournalEntryKind::HookDenial {
                        hook_name: hook.clone(),
                        operation: "llm".into(),
                        reason: reason.clone(),
                    })?;
                    return Err(RuntimeError::HookKill { hook, reason });
                }
                Err(e) => {
                    return Err(RuntimeError::HookError(e.to_string()));
                }
            }
        }

        // S019: Emit Token event for the response text (non-streaming path).
        // When Provider supports streaming, tokens are emitted incrementally.
        if !response.message.content.is_empty() {
            self.sink.emit(ActivityEvent::Token {
                text: response.message.content.clone(),
            });
        }

        // 6. Journal LlmResponse — must succeed before we expose the LLM output
        // to the caller / tool dispatch below.
        self.journal_entry(JournalEntryKind::LlmResponse {
            model: response.model.clone(),
            token_usage: response.token_usage.clone(),
            finish_reason: format!("{:?}", response.finish_reason),
            assistant_message: Some(response.message.clone()),
        })?;

        // 7. Update budget
        self.budget.used_tokens = self
            .budget
            .used_tokens
            .saturating_add(response.token_usage.total());
        self.budget.used_turns += 1;
        self.sync_proc_state();

        // Budget remaining gauges
        let remaining_turns = self.budget.max_turns.saturating_sub(self.budget.used_turns);
        let remaining_tokens = self
            .budget
            .max_tokens
            .saturating_sub(self.budget.used_tokens);
        tracing::info!(
            simulacra.agent.budget.remaining = remaining_turns as u64,
            simulacra.agent.budget.resource = "turns",
            "budget remaining"
        );
        tracing::info!(
            simulacra.agent.budget.remaining = remaining_tokens,
            simulacra.agent.budget.resource = "tokens",
            "budget remaining"
        );
        tracing::info!(simulacra.agent.turns = 1u64, "agent turn completed");

        // S010: Record OTel meter observations for turn completion
        {
            let meters = RuntimeMeters::get();
            let attrs = &[
                KeyValue::new("simulacra.agent.id", self.config.agent_id.0.clone()),
                KeyValue::new("gen_ai.request.model", self.config.model.clone()),
            ];
            meters.turns_counter.add(1, attrs);
            meters
                .budget_tokens_used
                .add(response.token_usage.total(), attrs);
            meters.budget_turns_used.add(1, attrs);
        }

        // 8. Append assistant message
        messages.push(response.message.clone());

        // 9. If no tool calls, return Complete
        if response.message.tool_calls.is_empty() {
            // S019: Emit TurnComplete on every return path
            self.sink.emit(ActivityEvent::TurnComplete);
            return Ok(TurnResult::Complete(response.message));
        }

        // 10. Dispatch tool calls
        let mut tool_results = Vec::new();
        for tc in &response.message.tool_calls {
            tracing::info!(
                "gen_ai.tool.message" = format!("tool_call: {}", tc.name),
                tool_name = tc.name.as_str(),
                tool_call_id = tc.id.as_str(),
            );

            // S019: Emit ToolStart before execution
            self.sink.emit(ActivityEvent::ToolStart {
                tool_call_id: tc.id.clone(),
                name: tc.name.clone(),
                arguments: tc.arguments.clone(),
            });

            self.journal_entry(JournalEntryKind::ToolCall {
                tool_call_id: Some(tc.id.clone()),
                tool_name: tc.name.clone(),
                arguments: tc.arguments.clone(),
            })?;
            self.consume_replay_entry(&JournalEntryKind::ToolCall {
                tool_call_id: Some(tc.id.clone()),
                tool_name: tc.name.clone(),
                arguments: tc.arguments.clone(),
            })?;

            let tool_start = Instant::now();
            let replayed_result = self.take_replay_tool_result(&tc.id, &tc.name)?;
            let (content, is_error) = match replayed_result {
                Some(result) => result,
                None => {
                    execute_tool_live(
                        &self.tools,
                        tc,
                        &self.config.capability,
                        &self.config.agent_id.0,
                    )
                    .await
                }
            };
            let tool_duration_ms = tool_start.elapsed().as_millis() as u64;

            // S019: Emit ToolOutput for each line of tool result content.
            // The agent loop owns the sink — tools return output through existing
            // channels, and the loop emits ToolOutput events per line.
            for line in content.lines() {
                self.sink.emit(ActivityEvent::ToolOutput {
                    tool_call_id: tc.id.clone(),
                    line: line.to_string(),
                });
            }

            // S019: Emit ToolFinish after execution
            self.sink.emit(ActivityEvent::ToolFinish {
                tool_call_id: tc.id.clone(),
                name: tc.name.clone(),
                is_error,
                duration_ms: tool_duration_ms,
                exit_code: None,
            });

            self.journal_entry(JournalEntryKind::ToolResult {
                tool_call_id: Some(tc.id.clone()),
                tool_name: tc.name.clone(),
                content: content.clone(),
                is_error,
            })?;

            let error_prefix = if is_error { "ERROR: " } else { "" };
            let tool_msg = Message {
                role: Role::Tool,
                content: format!("{error_prefix}{content}"),
                tool_calls: vec![],
                tool_call_id: Some(tc.id.clone()),
            };
            messages.push(tool_msg.clone());
            tool_results.push(tool_msg);
        }

        // S019: Emit TurnComplete on every return path
        self.sink.emit(ActivityEvent::TurnComplete);

        Ok(TurnResult::ToolCallsProcessed {
            assistant_message: response.message,
            tool_results,
        })
    }

    /// Run the loop: receive task -> [LLM -> tool calls -> journal -> repeat] -> exit.
    pub async fn run(&mut self, task: &str) -> Result<AgentLoopOutput, RuntimeError> {
        use tracing::Instrument;
        let agent_span = tracing::info_span!(
            "invoke_agent",
            "gen_ai.operation.name" = "invoke_agent",
            "gen_ai.agent.name" = self.config.agent_id.0.as_str(),
        );
        self.run_inner(task).instrument(agent_span).await
    }

    async fn run_inner(&mut self, task: &str) -> Result<AgentLoopOutput, RuntimeError> {
        // Default conversation: [system, user(task)]. Overridden below if the
        // replay journal carries a checkpoint with captured messages.
        let mut messages = vec![
            Message {
                role: Role::System,
                content: self.config.system_prompt.clone(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: task.to_string(),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ];
        let mut total_usage = TokenUsage::default();
        let tool_defs = self.tools.definitions();

        // S006/S005: Restore full agent state from the latest checkpoint in replay.
        //
        // A `CheckpointData` captures three things taken at checkpoint time:
        //   - `budget_snapshot`: budget counters (always restored)
        //   - `messages`: conversation state up to the checkpoint
        //   - `vfs_snapshot`: serialized VFS state
        //
        // Previously only `budget_snapshot` was honored, meaning replay-from-
        // checkpoint would start with a fresh [system, user] conversation and
        // an un-restored filesystem — silently diverging from the checkpointed
        // state. We now restore all three so replay resumes at the exact
        // point the checkpoint was taken (LangGraph-style fork-from-checkpoint).
        if let Some(ref replay) = self.replay {
            let mut latest_checkpoint: Option<CheckpointData> = None;
            for entry in replay.entries() {
                if let JournalEntryKind::Checkpoint { snapshot_data } = &entry.entry
                    && let Ok(checkpoint) = serde_json::from_slice::<CheckpointData>(snapshot_data)
                {
                    // Keep the latest checkpoint — replay resumes from the most
                    // recent snapshot, matching the checkpoint/fork semantics.
                    latest_checkpoint = Some(checkpoint);
                }
            }
            if let Some(checkpoint) = latest_checkpoint {
                self.budget = checkpoint.budget_snapshot;
                self.sync_proc_state();
                if !checkpoint.messages.is_empty() {
                    messages = checkpoint.messages;
                }
                if let Some(ref vfs_bytes) = checkpoint.vfs_snapshot {
                    if let Some(ref vfs) = self.vfs {
                        let snapshot = VfsSnapshot {
                            data: vfs_bytes.clone(),
                        };
                        if let Err(e) = vfs.restore(&snapshot) {
                            tracing::warn!(
                                error = %e,
                                "failed to restore vfs snapshot from checkpoint — replay may diverge"
                            );
                        }
                    } else {
                        tracing::warn!(
                            "checkpoint contains vfs_snapshot but AgentLoop has no VFS — \
                             vfs state will not be restored (call AgentLoop::set_vfs)"
                        );
                    }
                }
            }
        }

        // Track replay metrics for ratio gauge
        let total_replay_entries = self.replay.as_ref().map(|r| r.remaining()).unwrap_or(0);

        // 0 = unlimited (use u32::MAX as sentinel)
        let effective_max_turns = if self.config.max_turns == 0 {
            u32::MAX
        } else {
            self.config.max_turns
        };
        for _turn in 0..effective_max_turns {
            // 1. Check budget BEFORE the operation
            if let Err(exhausted) = self.budget.check_budget() {
                tracing::warn!(
                    simulacra.agent.budget.resource = %exhausted.resource,
                    simulacra.agent.budget.used = %exhausted.used,
                    simulacra.agent.budget.limit = %exhausted.limit,
                    "budget exhausted"
                );
                RuntimeMeters::get().budget_exhaustions.add(
                    1,
                    &[
                        KeyValue::new("simulacra.budget.resource", exhausted.resource.clone()),
                        KeyValue::new("simulacra.agent.id", self.config.agent_id.0.clone()),
                    ],
                );
                return Err(exhausted.into());
            }

            // 2. Journal TurnStart — before any side effect in this turn.
            self.journal_entry(JournalEntryKind::TurnStart)?;
            // Consume TurnStart from replay if before frontier
            self.consume_replay_entry(&JournalEntryKind::TurnStart)?;

            // 3. Compact context (0 = unlimited → use u64::MAX as sentinel)
            let remaining_tokens = if self.budget.max_tokens == 0 {
                u64::MAX
            } else {
                self.budget
                    .max_tokens
                    .saturating_sub(self.budget.used_tokens)
            };
            let compacted = self.context_strategy.compact(&messages, remaining_tokens);

            // 4. Journal LlmRequest — BEFORE invoking the provider.
            let llm_request = JournalEntryKind::LlmRequest {
                model: self.config.model.clone(),
                message_count: compacted.len(),
            };
            self.journal_entry(llm_request.clone())?;
            // Consume LlmRequest from replay if before frontier
            self.consume_replay_entry(&llm_request)?;

            // 5. Get LLM response — from replay if available, otherwise live
            // BEFORE hook
            if let Some(ref pipeline) = self.pipeline {
                let before_ctx = serde_json::json!({
                    "model": &self.config.model,
                    "message_count": compacted.len(),
                })
                .to_string();
                match pipeline.run_before(simulacra_hooks::verdict::Operation::Llm, &before_ctx) {
                    Ok((simulacra_hooks::Verdict::Continue(_), _)) => {}
                    Ok((simulacra_hooks::Verdict::Deny(reason), _)) => {
                        self.journal_entry(JournalEntryKind::HookDenial {
                            hook_name: "llm:before".into(),
                            operation: "llm".into(),
                            reason: reason.clone(),
                        })?;
                        return Err(RuntimeError::HookDenial(reason));
                    }
                    Ok((simulacra_hooks::Verdict::Kill(_), _)) => {
                        unreachable!("Kill is returned as Err from run_before")
                    }
                    Err(simulacra_hooks::HookError::Killed { hook, reason }) => {
                        self.journal_entry(JournalEntryKind::HookDenial {
                            hook_name: hook.clone(),
                            operation: "llm".into(),
                            reason: reason.clone(),
                        })?;
                        return Err(RuntimeError::HookKill { hook, reason });
                    }
                    Err(e) => {
                        return Err(RuntimeError::HookError(e.to_string()));
                    }
                }
            }

            let response = if self.has_replay_entry() {
                let kind = self.take_replay_entry()?;
                replay_llm_response(&kind)?
            } else {
                self.provider
                    .chat(&compacted, &tool_defs, &mut self.budget)
                    .await
                    .map_err(RuntimeError::from)?
            };

            // AFTER hook
            if let Some(ref pipeline) = self.pipeline {
                let after_ctx = serde_json::json!({
                    "model": &response.model,
                    "content": &response.message.content,
                    "tool_calls": &response.message.tool_calls,
                    "usage": {
                        "input_tokens": response.token_usage.input_tokens,
                        "output_tokens": response.token_usage.output_tokens,
                    },
                })
                .to_string();
                match pipeline.run_after(simulacra_hooks::verdict::Operation::Llm, &after_ctx) {
                    Ok(_) => {}
                    Err(simulacra_hooks::HookError::Killed { hook, reason }) => {
                        self.journal_entry(JournalEntryKind::HookDenial {
                            hook_name: hook.clone(),
                            operation: "llm".into(),
                            reason: reason.clone(),
                        })?;
                        return Err(RuntimeError::HookKill { hook, reason });
                    }
                    Err(e) => {
                        return Err(RuntimeError::HookError(e.to_string()));
                    }
                }
            }

            // S019: Emit Token event for the response text (non-streaming path).
            // When Provider supports streaming, tokens are emitted incrementally.
            // Extended thinking blocks emit ThinkStart, ThinkDelta, ThinkEnd
            // with think_duration_ms and think_tokens derived from the stream.
            if !response.message.content.is_empty() {
                self.sink.emit(ActivityEvent::Token {
                    text: response.message.content.clone(),
                });
            }

            // 6. Journal LlmResponse BEFORE tool dispatch / returning result.
            self.journal_entry(JournalEntryKind::LlmResponse {
                model: response.model.clone(),
                token_usage: response.token_usage.clone(),
                finish_reason: format!("{:?}", response.finish_reason),
                assistant_message: Some(response.message.clone()),
            })?;

            // 7. Update usage — AgentLoop owns all budget accounting
            total_usage.input_tokens += response.token_usage.input_tokens;
            total_usage.output_tokens += response.token_usage.output_tokens;
            self.budget.used_tokens = self
                .budget
                .used_tokens
                .saturating_add(response.token_usage.total());
            self.budget.used_turns += 1;
            self.sync_proc_state();

            // S006: Emit budget remaining gauge after each budget-consuming operation
            let remaining_turns = self.budget.max_turns.saturating_sub(self.budget.used_turns);
            let remaining_tokens = self
                .budget
                .max_tokens
                .saturating_sub(self.budget.used_tokens);
            tracing::info!(
                simulacra.agent.budget.remaining = remaining_turns as u64,
                simulacra.agent.budget.resource = "turns",
                "budget remaining"
            );
            tracing::info!(
                simulacra.agent.budget.remaining = remaining_tokens,
                simulacra.agent.budget.resource = "tokens",
                "budget remaining"
            );

            // S009: Emit simulacra.agent.turns counter for per-agent turn tracking
            tracing::info!(simulacra.agent.turns = 1u64, "agent turn completed");

            // S010: Record OTel meter observations for turn completion
            {
                let meters = RuntimeMeters::get();
                let attrs = &[
                    KeyValue::new("simulacra.agent.id", self.config.agent_id.0.clone()),
                    KeyValue::new("gen_ai.request.model", self.config.model.clone()),
                ];
                meters.turns_counter.add(1, attrs);
                meters
                    .budget_tokens_used
                    .add(response.token_usage.total(), attrs);
                meters.budget_turns_used.add(1, attrs);
            }

            // 8. Append assistant message
            messages.push(response.message.clone());

            // 9. If no tool calls, exit Complete
            if response.message.tool_calls.is_empty() {
                self.emit_replay_ratio(total_replay_entries);
                let exit_reason = ExitReason::Complete;
                // S009: Log agent completion at INFO with exit reason and token total
                tracing::info!(
                    "gen_ai.agent.name" = self.config.agent_id.0.as_str(),
                    "simulacra.agent.exit_reason" = format!("{:?}", exit_reason).as_str(),
                    "simulacra.agent.token_total" = total_usage.total(),
                    "agent completed"
                );
                return Ok(AgentLoopOutput {
                    exit_reason,
                    messages,
                    token_usage: total_usage,
                    used_turns: self.budget.used_turns,
                    used_cost: self.budget.used_cost,
                });
            }

            // 10. Dispatch tool calls
            for tc in &response.message.tool_calls {
                // S010: Emit gen_ai.tool.message event for OTel observability
                tracing::info!(
                    "gen_ai.tool.message" = format!("tool_call: {}", tc.name),
                    tool_name = tc.name.as_str(),
                    tool_call_id = tc.id.as_str(),
                );

                // S019: Emit ToolStart before execution
                self.sink.emit(ActivityEvent::ToolStart {
                    tool_call_id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                });

                // Journal ToolCall BEFORE execution.
                self.journal_entry(JournalEntryKind::ToolCall {
                    tool_call_id: Some(tc.id.clone()),
                    tool_name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })?;
                // Consume ToolCall from replay if before frontier
                self.consume_replay_entry(&JournalEntryKind::ToolCall {
                    tool_call_id: Some(tc.id.clone()),
                    tool_name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })?;

                // Get tool result — from replay if available, otherwise live
                let tool_start = Instant::now();
                let replayed_result = self.take_replay_tool_result(&tc.id, &tc.name)?;
                let (content, is_error) = match replayed_result {
                    Some(result) => result,
                    None => {
                        execute_tool_live(
                            &self.tools,
                            tc,
                            &self.config.capability,
                            &self.config.agent_id.0,
                        )
                        .await
                    }
                };
                let tool_duration_ms = tool_start.elapsed().as_millis() as u64;

                // S019: Emit ToolOutput for each line of tool result content.
                for line in content.lines() {
                    self.sink.emit(ActivityEvent::ToolOutput {
                        tool_call_id: tc.id.clone(),
                        line: line.to_string(),
                    });
                }

                // S019: Emit ToolFinish after execution
                self.sink.emit(ActivityEvent::ToolFinish {
                    tool_call_id: tc.id.clone(),
                    name: tc.name.clone(),
                    is_error,
                    duration_ms: tool_duration_ms,
                    exit_code: None,
                });

                // Journal ToolResult BEFORE returning to the loop (S005).
                self.journal_entry(JournalEntryKind::ToolResult {
                    tool_call_id: Some(tc.id.clone()),
                    tool_name: tc.name.clone(),
                    content: content.clone(),
                    is_error,
                })?;

                // 11. Append tool result as Tool message
                // is_error is preserved for the provider to signal tool failure to the model
                let error_prefix = if is_error { "ERROR: " } else { "" };
                messages.push(Message {
                    role: Role::Tool,
                    content: format!("{error_prefix}{content}"),
                    tool_calls: vec![],
                    tool_call_id: Some(tc.id.clone()),
                });
            }
        }

        // Max turns reached
        self.emit_replay_ratio(total_replay_entries);
        Ok(AgentLoopOutput {
            exit_reason: ExitReason::MaxTurns,
            messages,
            token_usage: total_usage,
            used_turns: self.budget.used_turns,
            used_cost: self.budget.used_cost,
        })
    }

    /// Emit thinking block events from a provider's extended thinking content.
    ///
    /// Called when the provider stream delivers thinking blocks. The agent loop
    /// measures think_duration_ms from ThinkStart to ThinkEnd and estimates
    /// think_tokens by dividing character count by 4 (approximate token count).
    #[allow(dead_code)]
    fn emit_thinking_events(&self, thinking_text: &str, think_start: Instant) {
        self.sink.emit(ActivityEvent::ThinkStart);
        // Emit the thinking text as ThinkDelta chunks
        self.sink.emit(ActivityEvent::ThinkDelta {
            text: thinking_text.to_string(),
        });
        let think_duration_ms = think_start.elapsed().as_millis() as u64;
        let think_tokens = (thinking_text.len() as u64) / 4;
        self.sink.emit(ActivityEvent::ThinkEnd {
            think_duration_ms,
            think_tokens,
        });
    }

    /// Emit the replay ratio gauge if replay was active.
    fn emit_replay_ratio(&self, total_replay_entries: usize) {
        if total_replay_entries > 0 {
            let replayed = self
                .replay
                .as_ref()
                .map(|r| total_replay_entries - r.remaining())
                .unwrap_or(0);
            let ratio = replayed as f64 / total_replay_entries as f64;
            tracing::info!(
                simulacra.journal.replay.ratio = ratio,
                "journal replay ratio"
            );
        }
    }

    /// Append a journal entry with the injected clock.
    ///
    /// ARCHITECTURE.md "Journal Before Return" makes journal append part of
    /// the side-effect contract: every operation must have its entry written
    /// before the result is returned. A failed append means replay would
    /// diverge silently, so we propagate the error to the caller; the caller
    /// must `?` this and abort the turn if journaling is critical for the
    /// next step (LLM calls, tool executions, hook denials).
    fn journal_entry(&self, kind: JournalEntryKind) -> Result<(), RuntimeError> {
        let timestamp_ms = self.clock.now_ms();
        let entry_kind_name = entry_kind_name(&kind);
        let mode = if self.has_replay_entry() {
            "replayed"
        } else {
            "live"
        };

        let _span = tracing::info_span!(
            "journal_append",
            "simulacra.operation.name" = "journal_append",
            "simulacra.journal.entry_kind" = entry_kind_name,
            "simulacra.journal.mode" = mode,
        )
        .entered();

        // S005: Emit counter event for journal entries by kind
        tracing::info!(
            simulacra.journal.entries = 1u64,
            simulacra.journal.entry_kind = entry_kind_name,
            "journal entry appended"
        );

        let entry = JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.config.agent_id.clone(),
            timestamp_ms,
            entry: kind,
        };

        if let Err(e) = self.journal.append(entry) {
            self.journal_write_failures.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                error = %e,
                entry_kind = entry_kind_name,
                "journal append failed — aborting turn to preserve replay determinism"
            );
            return Err(RuntimeError::JournalAppendFailed {
                entry_kind: entry_kind_name,
                source: e,
            });
        }
        Ok(())
    }

    /// Check if replay has an entry available before the frontier.
    fn has_replay_entry(&self) -> bool {
        self.replay.as_ref().is_some_and(|r| !r.at_frontier())
    }

    /// Consume and discard the next replay entry after verifying it matches
    /// the entry we just journaled. A shifted journal must fail at the first
    /// divergence instead of silently advancing the replay cursor.
    fn consume_replay_entry(&mut self, expected: &JournalEntryKind) -> Result<(), RuntimeError> {
        if let Some(ref mut replay) = self.replay
            && !replay.at_frontier()
        {
            let actual = replay.next_recorded().cloned().ok_or_else(|| {
                RuntimeError::Journal(simulacra_types::JournalError::Storage(
                    "replay frontier reached while consuming expected entry".into(),
                ))
            })?;
            if !replay_entries_match(expected, &actual) {
                return Err(RuntimeError::Journal(
                    simulacra_types::JournalError::Storage(format!(
                        "replay divergence: expected {} but found {}",
                        describe_replay_entry(expected),
                        describe_replay_entry(&actual)
                    )),
                ));
            }
        }
        Ok(())
    }

    /// Take (clone) the next replay entry kind.
    fn take_replay_entry(&mut self) -> Result<JournalEntryKind, RuntimeError> {
        self.replay
            .as_mut()
            .and_then(|r| r.next_recorded().cloned())
            .ok_or_else(|| {
                RuntimeError::Journal(simulacra_types::JournalError::Storage(
                    "take_replay_entry called but no replay entry available".into(),
                ))
            })
    }

    /// Consume replay entries after a ToolCall until its final ToolResult appears.
    fn take_replay_tool_result(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
    ) -> Result<Option<(String, bool)>, RuntimeError> {
        if !self.has_replay_entry() {
            return Ok(None);
        }

        let Some(replay) = self.replay.as_ref() else {
            return Ok(None);
        };
        let start = replay.position();
        let entries = replay.entries();
        let mut selected: Option<usize> = None;
        let allow_legacy_tool_result_match =
            entries.get(start.saturating_sub(1)).is_some_and(|entry| {
                matches!(
                    entry.entry,
                    JournalEntryKind::ToolCall {
                        tool_call_id: None,
                        ..
                    }
                )
            });

        for (offset, entry) in entries[start..].iter().enumerate() {
            match &entry.entry {
                JournalEntryKind::ToolResult {
                    tool_call_id: Some(recorded_id),
                    tool_name: recorded,
                    ..
                } if recorded_id == tool_call_id && recorded == tool_name => {
                    selected = Some(offset);
                    break;
                }
                // Backward compatibility for old journals that predate
                // tool_call_id. New nested sandbox results do not set ids, so
                // they no longer collide with the top-level ToolResult.
                JournalEntryKind::ToolResult {
                    tool_call_id: None,
                    tool_name: recorded,
                    ..
                } if allow_legacy_tool_result_match && recorded == tool_name => {
                    selected = Some(offset)
                }
                JournalEntryKind::ToolResult { .. }
                | JournalEntryKind::ShellCommand { .. }
                | JournalEntryKind::CodeExecution { .. }
                | JournalEntryKind::SubAgentSpawned { .. }
                | JournalEntryKind::SubAgentCompleted { .. }
                | JournalEntryKind::FileWrite { .. }
                | JournalEntryKind::HttpRequest { .. }
                | JournalEntryKind::Checkpoint { .. }
                | JournalEntryKind::HookDenial { .. }
                | JournalEntryKind::HookKill { .. } => {}
                other => {
                    if let Some(selected) = selected {
                        return self.consume_replay_tool_result_at(selected);
                    }
                    return Err(RuntimeError::Journal(
                        simulacra_types::JournalError::Storage(format!(
                            "expected ToolResult for {tool_name} ({tool_call_id}) during replay, found {}",
                            describe_replay_entry(other)
                        )),
                    ));
                }
            }
        }

        if let Some(selected) = selected {
            return self.consume_replay_tool_result_at(selected);
        }

        Err(RuntimeError::Journal(
            simulacra_types::JournalError::Storage(format!(
                "expected ToolResult for {tool_name} ({tool_call_id}) during replay, reached replay frontier"
            )),
        ))
    }

    fn consume_replay_tool_result_at(
        &mut self,
        offset: usize,
    ) -> Result<Option<(String, bool)>, RuntimeError> {
        let mut selected = None;
        for idx in 0..=offset {
            let kind = self
                .replay
                .as_mut()
                .and_then(|r| r.next_recorded().cloned())
                .ok_or_else(|| {
                    RuntimeError::Journal(simulacra_types::JournalError::Storage(
                        "replay frontier reached while consuming tool result".into(),
                    ))
                })?;
            if idx == offset {
                selected = Some(replay_tool_result(&kind)?);
            }
        }
        Ok(selected)
    }
}

/// Return the variant name of a JournalEntryKind for telemetry.
fn entry_kind_name(kind: &JournalEntryKind) -> &'static str {
    match kind {
        JournalEntryKind::TurnStart => "TurnStart",
        JournalEntryKind::LlmRequest { .. } => "LlmRequest",
        JournalEntryKind::LlmResponse { .. } => "LlmResponse",
        JournalEntryKind::ToolCall { .. } => "ToolCall",
        JournalEntryKind::ToolResult { .. } => "ToolResult",
        JournalEntryKind::ShellCommand { .. } => "ShellCommand",
        JournalEntryKind::CodeExecution { .. } => "CodeExecution",
        JournalEntryKind::SubAgentSpawned { .. } => "SubAgentSpawned",
        JournalEntryKind::SubAgentCompleted { .. } => "SubAgentCompleted",
        JournalEntryKind::FileWrite { .. } => "FileWrite",
        JournalEntryKind::HttpRequest { .. } => "HttpRequest",
        JournalEntryKind::Checkpoint { .. } => "Checkpoint",
        JournalEntryKind::HookDenial { .. } => "HookDenial",
        JournalEntryKind::HookKill { .. } => "HookKill",
    }
}

fn replay_entries_match(expected: &JournalEntryKind, actual: &JournalEntryKind) -> bool {
    match (expected, actual) {
        (JournalEntryKind::TurnStart, JournalEntryKind::TurnStart) => true,
        (
            JournalEntryKind::LlmRequest {
                model: expected_model,
                message_count: expected_count,
            },
            JournalEntryKind::LlmRequest {
                model: actual_model,
                message_count: actual_count,
            },
        ) => expected_model == actual_model && expected_count == actual_count,
        (
            JournalEntryKind::ToolCall {
                tool_call_id: expected_id,
                tool_name: expected_tool,
                arguments: expected_args,
            },
            JournalEntryKind::ToolCall {
                tool_call_id: actual_id,
                tool_name: actual_tool,
                arguments: actual_args,
            },
        ) => {
            let ids_match = match (expected_id, actual_id) {
                (Some(expected), Some(actual)) => expected == actual,
                // Backward compatibility: old journals did not record ids.
                (_, None) => true,
                (None, Some(_)) => true,
            };
            ids_match && expected_tool == actual_tool && expected_args == actual_args
        }
        _ => false,
    }
}

fn describe_replay_entry(kind: &JournalEntryKind) -> String {
    match kind {
        JournalEntryKind::LlmRequest {
            model,
            message_count,
        } => format!("LlmRequest(model={model}, message_count={message_count})"),
        JournalEntryKind::ToolCall {
            tool_call_id,
            tool_name,
            arguments,
        } => format!(
            "ToolCall(tool_call_id={}, tool_name={tool_name}, arguments={arguments})",
            tool_call_id.as_deref().unwrap_or("<legacy>")
        ),
        other => entry_kind_name(other).to_string(),
    }
}

/// Extract a ProviderResponse from a replayed LlmResponse journal entry.
fn replay_llm_response(
    kind: &JournalEntryKind,
) -> Result<simulacra_types::ProviderResponse, RuntimeError> {
    if let JournalEntryKind::LlmResponse {
        model,
        token_usage,
        finish_reason,
        assistant_message,
    } = kind
    {
        let fr = match finish_reason.as_str() {
            "EndTurn" => simulacra_types::FinishReason::EndTurn,
            "ToolUse" => simulacra_types::FinishReason::ToolUse,
            "MaxTokens" => simulacra_types::FinishReason::MaxTokens,
            "StopSequence" => simulacra_types::FinishReason::StopSequence,
            _ => simulacra_types::FinishReason::EndTurn,
        };

        // Use the stored assistant message (with tool_calls) if available,
        // otherwise reconstruct a minimal message (backwards compat with older journals).
        let message = assistant_message.clone().unwrap_or_else(|| Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![],
            tool_call_id: None,
        });

        Ok(simulacra_types::ProviderResponse {
            message,
            token_usage: token_usage.clone(),
            finish_reason: fr,
            provider_response_id: None,
            model: model.clone(),
        })
    } else {
        let actual_kind = entry_kind_name(kind);
        tracing::error!(
            expected = "LlmResponse",
            actual = actual_kind,
            "replay divergence: expected LlmResponse but got {actual_kind}"
        );
        Err(RuntimeError::Journal(
            simulacra_types::JournalError::Storage(format!(
                "expected LlmResponse during replay, got {kind:?}"
            )),
        ))
    }
}

/// Extract tool result from a replayed ToolResult journal entry.
fn replay_tool_result(kind: &JournalEntryKind) -> Result<(String, bool), RuntimeError> {
    if let JournalEntryKind::ToolResult {
        content, is_error, ..
    } = kind
    {
        Ok((content.clone(), *is_error))
    } else {
        Err(RuntimeError::Journal(
            simulacra_types::JournalError::Storage(format!(
                "expected ToolResult during replay, got {kind:?}"
            )),
        ))
    }
}

/// Execute a tool call live (not from replay).
async fn execute_tool_live(
    tools: &ToolRegistry,
    tc: &simulacra_types::ToolCallMessage,
    capability: &CapabilityToken,
    agent_name: &str,
) -> (String, bool) {
    let result = tools.call(&tc.name, tc.arguments.clone(), capability).await;
    match result {
        Ok(val) => {
            // If the tool returned JSON with an "error" field, treat it as
            // an error so the agent loop surfaces it with the ERROR: prefix.
            let is_error = val.is_object() && val.get("error").is_some();
            (val.to_string(), is_error)
        }
        Err(ref e @ simulacra_types::ToolError::CapabilityDenied(ref denied)) => {
            tracing::warn!(
                simulacra.capability.operation = %denied.operation,
                simulacra.capability.reason = %denied.reason,
                simulacra.capability.denials = "1",
                gen_ai.agent.name = agent_name,
                "capability denied"
            );
            (e.to_string(), true)
        }
        Err(e) => (e.to_string(), true),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryJournalStorage;
    use rust_decimal::Decimal;
    use simulacra_types::{
        FinishReason, JournalEntryKind, ProviderError, ProviderResponse, ToolCallMessage,
        ToolDefinition,
    };
    use std::sync::Mutex;

    // -----------------------------------------------------------------------
    // Fakes
    // -----------------------------------------------------------------------

    /// A fake provider that returns canned responses from a Vec, in order.
    struct FakeProvider {
        responses: Mutex<Vec<ProviderResponse>>,
    }

    impl FakeProvider {
        fn new(responses: Vec<ProviderResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl Provider for FakeProvider {
        fn chat<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a [ToolDefinition],
            _budget: &'a mut ResourceBudget,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                let mut responses = self
                    .responses
                    .lock()
                    .map_err(|e| ProviderError::Other(format!("lock poisoned: {e}")))?;
                if responses.is_empty() {
                    return Err(ProviderError::Other(
                        "FakeProvider: no more canned responses".into(),
                    ));
                }
                Ok(responses.remove(0))
            })
        }
    }

    /// A pass-through context strategy that returns messages unchanged.
    struct PassthroughContext;

    impl ContextStrategy for PassthroughContext {
        fn compact(&self, messages: &[Message], _token_limit: u64) -> Vec<Message> {
            messages.to_vec()
        }
    }

    /// A context strategy that truncates to only system + last N messages.
    struct TruncatingContext {
        keep_recent: usize,
    }

    impl ContextStrategy for TruncatingContext {
        fn compact(&self, messages: &[Message], _token_limit: u64) -> Vec<Message> {
            if messages.is_empty() {
                return vec![];
            }
            let mut result = vec![];
            if messages[0].role == Role::System {
                result.push(messages[0].clone());
                let rest = &messages[1..];
                let start = rest.len().saturating_sub(self.keep_recent);
                result.extend_from_slice(&rest[start..]);
            } else {
                let start = messages.len().saturating_sub(self.keep_recent);
                result.extend_from_slice(&messages[start..]);
            }
            result
        }
    }

    /// A fake tool that just echoes its arguments.
    struct EchoTool;

    impl simulacra_types::Tool for EchoTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".into(),
                description: "Echoes input".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        fn call(
            &self,
            arguments: serde_json::Value,
            _capability: &CapabilityToken,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<serde_json::Value, simulacra_types::ToolError>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async move { Ok(arguments) })
        }
    }

    struct DenyShellTool;

    impl simulacra_types::Tool for DenyShellTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "deny_shell".into(),
                description: "Always returns a shell capability denial".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        fn call(
            &self,
            _arguments: serde_json::Value,
            _capability: &CapabilityToken,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<serde_json::Value, simulacra_types::ToolError>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                Err(simulacra_types::ToolError::CapabilityDenied(
                    simulacra_types::CapabilityDenied {
                        operation: "shell".into(),
                        reason: "shell capability not granted".into(),
                    },
                ))
            })
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn text_response(content: &str) -> ProviderResponse {
        ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: content.to_string(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            token_usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
            finish_reason: FinishReason::EndTurn,
            provider_response_id: Some("resp-1".into()),
            model: "test-model".into(),
        }
    }

    fn tool_call_response(tool_name: &str, args: serde_json::Value) -> ProviderResponse {
        ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCallMessage {
                    id: "tc-1".into(),
                    name: tool_name.into(),
                    arguments: args,
                }],
                tool_call_id: None,
            },
            token_usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
            },
            finish_reason: FinishReason::ToolUse,
            provider_response_id: Some("resp-2".into()),
            model: "test-model".into(),
        }
    }

    fn default_budget() -> ResourceBudget {
        ResourceBudget::new(100_000, 10, Decimal::new(100, 0), 5)
    }

    fn default_config() -> AgentLoopConfig {
        AgentLoopConfig {
            agent_id: AgentId("test-agent".into()),
            system_prompt: "You are a test agent.".into(),
            model: "test-model".into(),
            max_turns: 10,
            capability: CapabilityToken::default(),
        }
    }

    fn build_loop(
        provider: FakeProvider,
        tools: ToolRegistry,
        context_strategy: Box<dyn ContextStrategy>,
        journal: Arc<dyn JournalStorage>,
        budget: ResourceBudget,
    ) -> AgentLoop {
        AgentLoop::new(
            default_config(),
            Box::new(provider),
            tools,
            context_strategy,
            journal,
            budget,
            None,
            None,
        )
    }

    // -----------------------------------------------------------------------
    // Test 1: Simple text response — one turn, exits Complete
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn simple_text_response_exits_complete() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![text_response("Hello, world!")]);
        let mut agent = build_loop(
            provider,
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal.clone(),
            default_budget(),
        );

        let output = agent.run("Say hello").await.expect("run should succeed");

        assert_eq!(output.exit_reason, ExitReason::Complete);
        assert_eq!(output.token_usage.input_tokens, 10);
        assert_eq!(output.token_usage.output_tokens, 5);

        // Messages: system + user + assistant
        assert_eq!(output.messages.len(), 3);
        assert_eq!(output.messages[0].role, Role::System);
        assert_eq!(output.messages[1].role, Role::User);
        assert_eq!(output.messages[2].role, Role::Assistant);
        assert_eq!(output.messages[2].content, "Hello, world!");

        // Journal: TurnStart, LlmRequest, LlmResponse
        let entries = journal
            .read_all(&AgentId("test-agent".into()))
            .expect("read_all should succeed");
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[0].entry, JournalEntryKind::TurnStart));
        assert!(matches!(
            entries[1].entry,
            JournalEntryKind::LlmRequest { .. }
        ));
        assert!(matches!(
            entries[2].entry,
            JournalEntryKind::LlmResponse { .. }
        ));
    }

    #[tokio::test]
    async fn proc_budget_mirror_tracks_loop_owned_turn_and_token_updates() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![text_response("mirror update")]);
        let initial_budget = default_budget();
        let budget_mirror = Arc::new(Mutex::new(initial_budget.clone()));
        let turn_mirror = Arc::new(AtomicU64::new(0));
        let mut agent = build_loop(
            provider,
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal,
            initial_budget,
        );
        agent.set_proc_budget_mirror(Arc::clone(&budget_mirror), Arc::clone(&turn_mirror));

        let output = agent.run("sync /proc").await.expect("run should succeed");

        assert_eq!(output.used_turns, 1);
        assert_eq!(turn_mirror.load(Ordering::Relaxed), 1);
        let mirrored = budget_mirror.lock().unwrap().clone();
        assert_eq!(mirrored.used_turns, 1);
        assert_eq!(mirrored.used_tokens, 15);
    }

    // -----------------------------------------------------------------------
    // Test 2: Tool call + response — two turns
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn tool_call_then_text_response() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![
            tool_call_response("echo", serde_json::json!({"msg": "hi"})),
            text_response("Done!"),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(EchoTool));

        let mut agent = build_loop(
            provider,
            tools,
            Box::new(PassthroughContext),
            journal.clone(),
            default_budget(),
        );

        let output = agent
            .run("Use the echo tool")
            .await
            .expect("run should succeed");

        assert_eq!(output.exit_reason, ExitReason::Complete);
        // Usage: turn1 (20+10) + turn2 (10+5) = 30 input, 15 output
        assert_eq!(output.token_usage.input_tokens, 30);
        assert_eq!(output.token_usage.output_tokens, 15);

        // Messages: system + user + assistant(tool_call) + tool_result + assistant(text)
        assert_eq!(output.messages.len(), 5);
        assert_eq!(output.messages[2].role, Role::Assistant);
        assert!(!output.messages[2].tool_calls.is_empty());
        assert_eq!(output.messages[3].role, Role::Tool);
        assert_eq!(output.messages[4].role, Role::Assistant);
        assert_eq!(output.messages[4].content, "Done!");

        // Journal: TurnStart, LlmRequest, LlmResponse, ToolCall, ToolResult, TurnStart, LlmRequest, LlmResponse
        let entries = journal
            .read_all(&AgentId("test-agent".into()))
            .expect("read_all should succeed");
        assert_eq!(entries.len(), 8);
        assert!(matches!(
            entries[3].entry,
            JournalEntryKind::ToolCall { .. }
        ));
        assert!(matches!(
            entries[4].entry,
            JournalEntryKind::ToolResult { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // Test 3: Budget exhaustion — max_turns=1 with tool call
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn max_turns_exits_max_turns() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        // Provider returns tool calls endlessly, but we cap at 1 turn
        let provider = FakeProvider::new(vec![tool_call_response(
            "echo",
            serde_json::json!({"msg": "loop"}),
        )]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(EchoTool));

        let mut config = default_config();
        config.max_turns = 1;

        let mut agent = AgentLoop::new(
            config,
            Box::new(provider),
            tools,
            Box::new(PassthroughContext),
            journal.clone(),
            default_budget(),
            None,
            None,
        );

        let output = agent.run("Loop forever").await.expect("run should succeed");
        assert_eq!(output.exit_reason, ExitReason::MaxTurns);
    }

    // -----------------------------------------------------------------------
    // Test 4: Journal entries written before return
    // -----------------------------------------------------------------------

    /// A provider that captures journal state at the moment `chat()` is called,
    /// proving temporal ordering: entries that should be journaled *before*
    /// the provider call will be visible in the snapshot.
    struct JournalCapturingProvider {
        responses: Mutex<Vec<ProviderResponse>>,
        journal: Arc<dyn JournalStorage>,
        agent_id: AgentId,
        /// Journal entries captured at the moment chat() is called.
        captured: Arc<Mutex<Option<Vec<JournalEntry>>>>,
    }

    impl Provider for JournalCapturingProvider {
        fn chat<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a [ToolDefinition],
            _budget: &'a mut ResourceBudget,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>>
                    + Send
                    + 'a,
            >,
        > {
            // Snapshot the journal at the moment the provider is called.
            let snapshot = self.journal.read_all(&self.agent_id).unwrap_or_default();
            *self.captured.lock().unwrap() = Some(snapshot);

            Box::pin(async {
                let mut responses = self
                    .responses
                    .lock()
                    .map_err(|e| ProviderError::Other(format!("lock poisoned: {e}")))?;
                if responses.is_empty() {
                    return Err(ProviderError::Other(
                        "JournalCapturingProvider: no more canned responses".into(),
                    ));
                }
                Ok(responses.remove(0))
            })
        }
    }

    #[tokio::test]
    async fn journal_entries_written_before_return() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        let agent_id = AgentId("test-agent".into());
        let journal_at_chat_time: Arc<Mutex<Option<Vec<JournalEntry>>>> =
            Arc::new(Mutex::new(None));

        let provider = JournalCapturingProvider {
            responses: Mutex::new(vec![text_response("Result")]),
            journal: journal.clone(),
            agent_id: agent_id.clone(),
            captured: journal_at_chat_time.clone(),
        };

        let mut agent = AgentLoop::new(
            default_config(),
            Box::new(provider),
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal.clone(),
            default_budget(),
            None,
            None,
        );

        let _ = agent.run("test").await.expect("run should succeed");

        // Verify temporal ordering: at the moment the provider's chat() was called,
        // TurnStart and LlmRequest must already be in the journal.
        let snapshot = journal_at_chat_time
            .lock()
            .unwrap()
            .take()
            .expect("provider should have captured journal state");

        let kinds_at_chat: Vec<&str> = snapshot
            .iter()
            .map(|e| match &e.entry {
                JournalEntryKind::TurnStart => "TurnStart",
                JournalEntryKind::LlmRequest { .. } => "LlmRequest",
                JournalEntryKind::LlmResponse { .. } => "LlmResponse",
                _ => "Other",
            })
            .collect();
        assert_eq!(
            kinds_at_chat,
            vec!["TurnStart", "LlmRequest"],
            "TurnStart and LlmRequest must be journaled BEFORE the provider call — \
             this proves journal-before-return ordering, not just post-hoc entry existence"
        );

        // Also verify the final journal state has all three entries in order.
        let final_entries = journal
            .read_all(&agent_id)
            .expect("read_all should succeed");
        let final_kinds: Vec<&str> = final_entries
            .iter()
            .map(|e| match &e.entry {
                JournalEntryKind::TurnStart => "TurnStart",
                JournalEntryKind::LlmRequest { .. } => "LlmRequest",
                JournalEntryKind::LlmResponse { .. } => "LlmResponse",
                _ => "Other",
            })
            .collect();
        assert_eq!(final_kinds, vec!["TurnStart", "LlmRequest", "LlmResponse"]);
    }

    // -----------------------------------------------------------------------
    // Test 5: Budget check before inference
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn exhausted_budget_returns_error_without_calling_provider() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        // Provider that would panic if called
        let provider = FakeProvider::new(vec![]);

        // Budget already exhausted: used_turns == max_turns
        let mut budget = ResourceBudget::new(100_000, 1, Decimal::new(100, 0), 5);
        budget.used_turns = 1;

        let mut agent = build_loop(
            provider,
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal.clone(),
            budget,
        );

        let result = agent.run("This should fail").await;
        assert!(result.is_err());

        // Provider should not have been called — no journal entries for LlmRequest
        let entries = journal
            .read_all(&AgentId("test-agent".into()))
            .expect("read_all should succeed");
        assert!(
            entries
                .iter()
                .all(|e| !matches!(e.entry, JournalEntryKind::LlmRequest { .. })),
            "provider should not have been called"
        );
    }

    // -----------------------------------------------------------------------
    // Test 6: Context compaction
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn context_strategy_compacts_messages() {
        let journal = Arc::new(InMemoryJournalStorage::new());

        // Use a truncating context strategy that keeps only system + last 1 message
        let context = TruncatingContext { keep_recent: 1 };

        // Two turns: tool call then text. The second call should receive compacted messages.
        let provider = FakeProvider::new(vec![
            tool_call_response("echo", serde_json::json!({"n": 1})),
            text_response("Final"),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(EchoTool));

        let mut agent = build_loop(
            provider,
            tools,
            Box::new(context),
            journal.clone(),
            default_budget(),
        );

        let output = agent
            .run("Use echo then finish")
            .await
            .expect("run should succeed");

        // The loop should complete successfully even with aggressive compaction
        assert_eq!(output.exit_reason, ExitReason::Complete);
        // Full message history preserved in output (compaction only affects provider input)
        assert_eq!(output.messages.len(), 5);
    }

    // -----------------------------------------------------------------------
    // Test 7: Token usage accumulates across turns
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn token_usage_accumulates_across_turns() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![
            tool_call_response("echo", serde_json::json!({})),
            text_response("done"),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(EchoTool));

        let mut agent = build_loop(
            provider,
            tools,
            Box::new(PassthroughContext),
            journal.clone(),
            default_budget(),
        );

        let output = agent.run("go").await.expect("run should succeed");

        // Turn 1: 20 in + 10 out; Turn 2: 10 in + 5 out
        assert_eq!(output.token_usage.input_tokens, 30);
        assert_eq!(output.token_usage.output_tokens, 15);
        assert_eq!(output.token_usage.total(), 45);
    }

    // -----------------------------------------------------------------------
    // Test 8: Budget tracks used_turns
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn budget_used_turns_increments() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![
            tool_call_response("echo", serde_json::json!({})),
            text_response("done"),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(EchoTool));

        let budget = default_budget();
        let mut agent = build_loop(
            provider,
            tools,
            Box::new(PassthroughContext),
            journal.clone(),
            budget,
        );

        let _ = agent.run("go").await.expect("run should succeed");

        // The budget is internal to agent, so we verify via journal: 2 TurnStart entries
        let entries = journal
            .read_all(&AgentId("test-agent".into()))
            .expect("read_all should succeed");
        let turn_starts = entries
            .iter()
            .filter(|e| matches!(e.entry, JournalEntryKind::TurnStart))
            .count();
        assert_eq!(turn_starts, 2);
    }

    #[tokio::test]
    async fn capability_denial_is_journaled_with_operation_details() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![
            tool_call_response("deny_shell", serde_json::json!({})),
            text_response("done"),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(DenyShellTool));

        let mut agent = build_loop(
            provider,
            tools,
            Box::new(PassthroughContext),
            journal.clone(),
            default_budget(),
        );

        let output = agent.run("try a denied tool").await.unwrap();

        let denied_result = journal
            .read_all(&AgentId("test-agent".into()))
            .unwrap()
            .into_iter()
            .find_map(|entry| match entry.entry {
                JournalEntryKind::ToolResult {
                    tool_name,
                    content,
                    is_error,
                    ..
                } if tool_name == "deny_shell" => Some((content, is_error)),
                _ => None,
            })
            .expect("expected a journaled tool result for the denied capability");

        assert_eq!(output.exit_reason, ExitReason::Complete);
        assert!(
            denied_result.1,
            "capability denial must be journaled as an error"
        );
        assert!(
            denied_result.0.contains("shell"),
            "journaled denial should include the denied operation"
        );
        assert!(
            denied_result.0.contains("shell capability not granted"),
            "journaled denial should include the denial reason"
        );
    }

    // -----------------------------------------------------------------------
    // S005: Injectable clock produces deterministic timestamps
    // -----------------------------------------------------------------------

    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    #[tokio::test]
    async fn injectable_clock_produces_deterministic_timestamps() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        let provider = FakeProvider::new(vec![text_response("Hello!")]);

        let mut agent = AgentLoop::with_clock_and_replay(
            default_config(),
            Box::new(provider),
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal.clone(),
            default_budget(),
            Box::new(FixedClock(42_000)),
            None,
        );

        let _ = agent.run("test").await.expect("run should succeed");

        let entries = journal
            .read_all(&AgentId("test-agent".into()))
            .expect("read_all should succeed");
        // All entries should have the fixed timestamp
        for entry in &entries {
            assert_eq!(
                entry.timestamp_ms, 42_000,
                "all journal entries should use the injected clock"
            );
        }
    }

    // -----------------------------------------------------------------------
    // S005: Replay with recorded LLM response does not make a real API call
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn replay_with_recorded_llm_response_skips_provider() {
        let journal = Arc::new(InMemoryJournalStorage::new());

        // A provider that panics if called — proves replay skips it
        struct PanickingProvider;

        impl Provider for PanickingProvider {
            fn chat<'a>(
                &'a self,
                _messages: &'a [Message],
                _tools: &'a [ToolDefinition],
                _budget: &'a mut ResourceBudget,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<ProviderResponse, simulacra_types::ProviderError>,
                        > + Send
                        + 'a,
                >,
            > {
                panic!("Provider::chat should not be called during replay");
            }
        }

        // Build a replay journal that represents one complete turn:
        // TurnStart, LlmRequest, LlmResponse (with EndTurn and no tool calls)
        let agent_id = AgentId("test-agent".into());
        let replay_entries = vec![
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 1000,
                entry: JournalEntryKind::TurnStart,
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 1001,
                entry: JournalEntryKind::LlmRequest {
                    model: "test-model".into(),
                    message_count: 2,
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 1002,
                entry: JournalEntryKind::LlmResponse {
                    model: "test-model".into(),
                    token_usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                    finish_reason: "EndTurn".into(),
                    assistant_message: Some(Message {
                        role: Role::Assistant,
                        content: "Replayed answer".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                    }),
                },
            },
        ];

        let mut agent = AgentLoop::with_clock_and_replay(
            default_config(),
            Box::new(PanickingProvider),
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal.clone(),
            default_budget(),
            Box::new(FixedClock(2000)),
            Some(replay_entries),
        );

        // This should succeed without calling the provider
        let output = agent
            .run("replayed task")
            .await
            .expect("replay should succeed");

        assert_eq!(output.exit_reason, ExitReason::Complete);
        assert_eq!(output.token_usage.input_tokens, 10);
        assert_eq!(output.token_usage.output_tokens, 5);
    }

    #[tokio::test]
    async fn replay_fails_immediately_when_turn_start_entry_is_shifted() {
        struct PanickingProvider;

        impl Provider for PanickingProvider {
            fn chat<'a>(
                &'a self,
                _messages: &'a [Message],
                _tools: &'a [ToolDefinition],
                _budget: &'a mut ResourceBudget,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>>
                        + Send
                        + 'a,
                >,
            > {
                panic!("Provider::chat should not be called after replay divergence");
            }
        }

        let journal = Arc::new(InMemoryJournalStorage::new());
        let replay_entries = vec![JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 1,
            entry: JournalEntryKind::LlmRequest {
                model: "test-model".into(),
                message_count: 2,
            },
        }];

        let mut agent = AgentLoop::with_clock_and_replay(
            default_config(),
            Box::new(PanickingProvider),
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal,
            default_budget(),
            Box::new(FixedClock(2000)),
            Some(replay_entries),
        );

        let error = agent
            .run("replayed task")
            .await
            .expect_err("shifted replay should fail before provider call");
        let message = error.to_string();
        assert!(
            message.contains("replay divergence")
                && message.contains("TurnStart")
                && message.contains("LlmRequest"),
            "unexpected error: {message}"
        );
    }

    #[tokio::test]
    async fn replay_fails_when_recorded_tool_call_does_not_match_live_tool_call() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        let assistant_message =
            tool_call_response("echo", serde_json::json!({"msg": "live"})).message;
        let replay_entries = vec![
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 1,
                entry: JournalEntryKind::TurnStart,
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 2,
                entry: JournalEntryKind::LlmRequest {
                    model: "test-model".into(),
                    message_count: 2,
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 3,
                entry: JournalEntryKind::LlmResponse {
                    model: "test-model".into(),
                    token_usage: TokenUsage {
                        input_tokens: 20,
                        output_tokens: 10,
                    },
                    finish_reason: "ToolUse".into(),
                    assistant_message: Some(assistant_message),
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 4,
                entry: JournalEntryKind::ToolCall {
                    tool_call_id: Some("tc-1".into()),
                    tool_name: "echo".into(),
                    arguments: serde_json::json!({"msg": "recorded"}),
                },
            },
        ];

        let mut tools = ToolRegistry::new();
        tools.register(Box::new(EchoTool));
        let mut agent = AgentLoop::with_clock_and_replay(
            default_config(),
            Box::new(FakeProvider::new(vec![])),
            tools,
            Box::new(PassthroughContext),
            journal,
            default_budget(),
            Box::new(FixedClock(2000)),
            Some(replay_entries),
        );
        let mut messages = vec![
            Message {
                role: Role::System,
                content: "system".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: "use echo".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ];

        let error = agent
            .run_single_turn(&mut messages)
            .await
            .expect_err("mismatched ToolCall arguments should fail replay");
        let message = error.to_string();
        assert!(
            message.contains("replay divergence")
                && message.contains("ToolCall")
                && message.contains("recorded"),
            "unexpected error: {message}"
        );
    }

    #[tokio::test]
    async fn replay_tool_result_skips_nested_sandbox_entries_between_tool_call_and_final_result() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        let assistant_message =
            tool_call_response("echo", serde_json::json!({"msg": "live"})).message;
        let replay_entries = vec![
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 1,
                entry: JournalEntryKind::TurnStart,
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 2,
                entry: JournalEntryKind::LlmRequest {
                    model: "test-model".into(),
                    message_count: 2,
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 3,
                entry: JournalEntryKind::LlmResponse {
                    model: "test-model".into(),
                    token_usage: TokenUsage {
                        input_tokens: 20,
                        output_tokens: 10,
                    },
                    finish_reason: "ToolUse".into(),
                    assistant_message: Some(assistant_message),
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 4,
                entry: JournalEntryKind::ToolCall {
                    tool_call_id: Some("tc-1".into()),
                    tool_name: "echo".into(),
                    arguments: serde_json::json!({"msg": "live"}),
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 5,
                entry: JournalEntryKind::ShellCommand {
                    command: "node /workspace/script.js".into(),
                    exit_code: 0,
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 6,
                entry: JournalEntryKind::CodeExecution {
                    language: "javascript".into(),
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 7,
                entry: JournalEntryKind::FileWrite {
                    path: "/workspace/out.txt".into(),
                    size_bytes: 5,
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 8,
                entry: JournalEntryKind::HttpRequest {
                    method: "GET".into(),
                    url: "https://example.test/".into(),
                    status: 200,
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 9,
                entry: JournalEntryKind::ToolResult {
                    tool_call_id: None,
                    tool_name: "echo".into(),
                    content: "nested collision".into(),
                    is_error: false,
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 10,
                entry: JournalEntryKind::ToolResult {
                    tool_call_id: Some("tc-1".into()),
                    tool_name: "echo".into(),
                    content: "recorded final".into(),
                    is_error: false,
                },
            },
        ];

        let mut tools = ToolRegistry::new();
        tools.register(Box::new(EchoTool));
        let mut agent = AgentLoop::with_clock_and_replay(
            default_config(),
            Box::new(FakeProvider::new(vec![])),
            tools,
            Box::new(PassthroughContext),
            journal,
            default_budget(),
            Box::new(FixedClock(2000)),
            Some(replay_entries),
        );
        let mut messages = vec![
            Message {
                role: Role::System,
                content: "system".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: "use echo".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ];

        let result = agent
            .run_single_turn(&mut messages)
            .await
            .expect("replay should skip nested sandbox entries");

        match result {
            TurnResult::ToolCallsProcessed { tool_results, .. } => {
                assert_eq!(tool_results.len(), 1);
                assert_eq!(tool_results[0].content, "recorded final");
            }
            other => panic!("expected replayed tool call processing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn replay_fails_when_current_tool_result_id_is_missing_after_nested_collision() {
        let journal = Arc::new(InMemoryJournalStorage::new());
        let assistant_message =
            tool_call_response("echo", serde_json::json!({"msg": "live"})).message;
        let replay_entries = vec![
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 1,
                entry: JournalEntryKind::TurnStart,
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 2,
                entry: JournalEntryKind::LlmRequest {
                    model: "test-model".into(),
                    message_count: 2,
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 3,
                entry: JournalEntryKind::LlmResponse {
                    model: "test-model".into(),
                    token_usage: TokenUsage {
                        input_tokens: 20,
                        output_tokens: 10,
                    },
                    finish_reason: "ToolUse".into(),
                    assistant_message: Some(assistant_message),
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 4,
                entry: JournalEntryKind::ToolCall {
                    tool_call_id: Some("tc-1".into()),
                    tool_name: "echo".into(),
                    arguments: serde_json::json!({"msg": "live"}),
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 5,
                entry: JournalEntryKind::ToolResult {
                    tool_call_id: None,
                    tool_name: "echo".into(),
                    content: "nested same-name result".into(),
                    is_error: false,
                },
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 6,
                entry: JournalEntryKind::LlmRequest {
                    model: "test-model".into(),
                    message_count: 4,
                },
            },
        ];

        let mut tools = ToolRegistry::new();
        tools.register(Box::new(EchoTool));
        let mut agent = AgentLoop::with_clock_and_replay(
            default_config(),
            Box::new(FakeProvider::new(vec![])),
            tools,
            Box::new(PassthroughContext),
            journal,
            default_budget(),
            Box::new(FixedClock(2000)),
            Some(replay_entries),
        );
        let mut messages = vec![
            Message {
                role: Role::System,
                content: "system".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: "use echo".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ];

        let error = agent
            .run_single_turn(&mut messages)
            .await
            .expect_err("replay must not use a nested same-name result without the current id");
        let message = error.to_string();
        assert!(
            message.contains("expected ToolResult for echo (tc-1)"),
            "unexpected error: {message}"
        );
    }

    #[tokio::test]
    async fn replay_frontier_after_recorded_request_transitions_to_live_provider() {
        // Edge case: when replay stops exactly at the provider boundary, the loop should switch
        // cleanly from recorded TurnStart/LlmRequest entries to a live provider call.
        struct CountingProvider {
            calls: Arc<Mutex<u32>>,
            response: ProviderResponse,
        }

        impl Provider for CountingProvider {
            fn chat<'a>(
                &'a self,
                _messages: &'a [Message],
                _tools: &'a [ToolDefinition],
                _budget: &'a mut ResourceBudget,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async move {
                    *self
                        .calls
                        .lock()
                        .map_err(|e| ProviderError::Other(format!("lock poisoned: {e}")))? += 1;
                    Ok(self.response.clone())
                })
            }
        }

        let calls = Arc::new(Mutex::new(0));
        let journal = Arc::new(InMemoryJournalStorage::new());
        let replay_entries = vec![
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 1000,
                entry: JournalEntryKind::TurnStart,
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 1001,
                entry: JournalEntryKind::LlmRequest {
                    model: "test-model".into(),
                    message_count: 2,
                },
            },
        ];

        let mut agent = AgentLoop::with_clock_and_replay(
            default_config(),
            Box::new(CountingProvider {
                calls: Arc::clone(&calls),
                response: text_response("live frontier response"),
            }),
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal,
            default_budget(),
            Box::new(FixedClock(2001)),
            Some(replay_entries),
        );

        let output = agent.run("cross the frontier").await.unwrap();

        assert_eq!(output.exit_reason, ExitReason::Complete);
        assert_eq!(output.messages[2].content, "live frontier response");
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn replay_tool_result_preserves_error_state() {
        // Edge case: replay of ToolResult entries must preserve is_error so resumed runs do not
        // silently reinterpret tool failures as successful tool outputs.
        let (content, is_error) = replay_tool_result(&JournalEntryKind::ToolResult {
            tool_call_id: Some("tc-1".into()),
            tool_name: "echo".into(),
            content: "tool exploded".into(),
            is_error: true,
        })
        .expect("tool results should replay");

        assert_eq!(content, "tool exploded");
        assert!(is_error);
    }

    #[tokio::test]
    async fn injected_clock_stays_deterministic_during_replay_resume() {
        // Edge case: resumed runs should timestamp newly appended entries from the injected clock,
        // even while earlier steps are being consumed from the replay journal.
        let journal = Arc::new(InMemoryJournalStorage::new());
        let replay_entries = vec![
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 10,
                entry: JournalEntryKind::TurnStart,
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 11,
                entry: JournalEntryKind::LlmRequest {
                    model: "test-model".into(),
                    message_count: 2,
                },
            },
        ];

        let mut agent = AgentLoop::with_clock_and_replay(
            default_config(),
            Box::new(FakeProvider::new(vec![text_response(
                "deterministic replay",
            )])),
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal.clone(),
            default_budget(),
            Box::new(FixedClock(42_424)),
            Some(replay_entries),
        );

        let _ = agent.run("resume").await.unwrap();

        let entries = journal.read_all(&AgentId("test-agent".into())).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|entry| entry.timestamp_ms == 42_424));
    }

    // -----------------------------------------------------------------------
    // S005: Replay iterator frontier behavior
    // -----------------------------------------------------------------------
    #[test]
    fn replay_iterator_frontier_behavior() {
        use crate::replay::JournalReplayIterator;

        let agent_id = AgentId("test-agent".into());
        let entries = vec![
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 1000,
                entry: JournalEntryKind::TurnStart,
            },
            JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 1001,
                entry: JournalEntryKind::LlmRequest {
                    model: "m".into(),
                    message_count: 1,
                },
            },
        ];

        let mut iter = JournalReplayIterator::new(entries);

        // Before consuming: not at frontier, 2 remaining
        assert!(!iter.at_frontier());
        assert_eq!(iter.remaining(), 2);
        assert_eq!(iter.position(), 0);

        // Peek doesn't advance
        assert!(iter.peek().is_some());
        assert_eq!(iter.position(), 0);

        // Consume first
        let first = iter.next_recorded();
        assert!(matches!(first, Some(JournalEntryKind::TurnStart)));
        assert_eq!(iter.position(), 1);
        assert_eq!(iter.remaining(), 1);

        // Consume second
        let second = iter.next_recorded();
        assert!(matches!(second, Some(JournalEntryKind::LlmRequest { .. })));
        assert_eq!(iter.position(), 2);

        // Now at frontier
        assert!(iter.at_frontier());
        assert_eq!(iter.remaining(), 0);
        assert!(iter.next_recorded().is_none());
    }

    // -----------------------------------------------------------------------
    // S010: OTel GenAI Semantic Convention Tests
    // -----------------------------------------------------------------------
    mod otel_span_tests {
        use super::*;
        use std::sync::Mutex as StdMutex;
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Debug, Clone)]
        struct CapturedSpan {
            name: String,
            fields: std::collections::HashMap<String, String>,
        }

        #[derive(Debug, Clone)]
        struct CapturedEvent {
            #[allow(dead_code)]
            name: String,
            level: String,
            current_span: Option<String>,
            fields: std::collections::HashMap<String, String>,
        }

        struct SpanCaptureLayer {
            spans: Arc<StdMutex<Vec<CapturedSpan>>>,
            events: Arc<StdMutex<Vec<CapturedEvent>>>,
        }

        impl<S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>
            tracing_subscriber::Layer<S> for SpanCaptureLayer
        {
            fn on_new_span(
                &self,
                attrs: &tracing::span::Attributes<'_>,
                _id: &tracing::span::Id,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = std::collections::HashMap::new();
                let mut visitor = FieldVisitor(&mut fields);
                attrs.record(&mut visitor);
                let span = CapturedSpan {
                    name: attrs.metadata().name().to_string(),
                    fields,
                };
                self.spans.lock().unwrap().push(span);
            }

            fn on_record(
                &self,
                id: &tracing::span::Id,
                values: &tracing::span::Record<'_>,
                ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let span_ref = ctx.span(id);
                if let Some(span_ref) = span_ref {
                    let span_name = span_ref.name().to_string();
                    let mut new_fields = std::collections::HashMap::new();
                    let mut visitor = FieldVisitor(&mut new_fields);
                    values.record(&mut visitor);
                    let mut spans = self.spans.lock().unwrap();
                    for captured in spans.iter_mut().rev() {
                        if captured.name == span_name {
                            for (k, v) in new_fields {
                                captured.fields.insert(k, v);
                            }
                            break;
                        }
                    }
                }
            }

            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = std::collections::HashMap::new();
                let mut visitor = FieldVisitor(&mut fields);
                event.record(&mut visitor);
                let captured = CapturedEvent {
                    name: event.metadata().name().to_string(),
                    level: event.metadata().level().to_string(),
                    current_span: ctx.lookup_current().map(|span| span.name().to_string()),
                    fields,
                };
                self.events.lock().unwrap().push(captured);
            }
        }

        struct FieldVisitor<'a>(&'a mut std::collections::HashMap<String, String>);

        impl tracing::field::Visit for FieldVisitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
            fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
            fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
        }

        #[allow(clippy::type_complexity)]
        fn setup_capture() -> (
            impl tracing::Subscriber + Send + Sync,
            Arc<StdMutex<Vec<CapturedSpan>>>,
            Arc<StdMutex<Vec<CapturedEvent>>>,
        ) {
            let spans = Arc::new(StdMutex::new(Vec::new()));
            let events = Arc::new(StdMutex::new(Vec::new()));
            let layer = SpanCaptureLayer {
                spans: Arc::clone(&spans),
                events: Arc::clone(&events),
            };
            let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
            (subscriber, spans, events)
        }

        // S010 Assertion: Agent spans use invoke_agent operation name
        #[tokio::test]
        async fn agent_loop_emits_invoke_agent_span() {
            let (subscriber, captured_spans, _events) = setup_capture();
            let journal = Arc::new(InMemoryJournalStorage::new());
            let provider = FakeProvider::new(vec![text_response("Hello!")]);
            let mut agent = build_loop(
                provider,
                ToolRegistry::new(),
                Box::new(PassthroughContext),
                journal,
                default_budget(),
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let _output = agent.run("Say hello").await.expect("run should succeed");

            let spans = captured_spans.lock().unwrap();
            let agent_span = spans
                .iter()
                .find(|s| {
                    s.fields.get("gen_ai.operation.name") == Some(&"invoke_agent".to_string())
                })
                .expect("expected a span with gen_ai.operation.name=invoke_agent");

            assert_eq!(
                agent_span.fields.get("gen_ai.agent.name"),
                Some(&"test-agent".to_string())
            );
        }

        // S010: Tool call events emit gen_ai.tool.message
        #[tokio::test]
        async fn tool_calls_emit_gen_ai_tool_message_event() {
            let (subscriber, _spans, captured_events) = setup_capture();
            let journal = Arc::new(InMemoryJournalStorage::new());
            let provider = FakeProvider::new(vec![
                tool_call_response("echo", serde_json::json!({"msg": "hi"})),
                text_response("Done!"),
            ]);
            let mut tools = ToolRegistry::new();
            tools.register(Box::new(EchoTool));

            let mut agent = build_loop(
                provider,
                tools,
                Box::new(PassthroughContext),
                journal,
                default_budget(),
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let _output = agent
                .run("Use echo tool")
                .await
                .expect("run should succeed");

            let events = captured_events.lock().unwrap();
            let tool_event = events
                .iter()
                .find(|e| e.fields.contains_key("gen_ai.tool.message"))
                .expect("expected an event with gen_ai.tool.message field");

            // Verify the tool name is in the event
            assert!(
                tool_event
                    .fields
                    .get("gen_ai.tool.message")
                    .unwrap()
                    .contains("echo"),
                "tool event should reference the tool name"
            );
        }

        #[tokio::test]
        async fn journal_append_span_records_entry_kind_and_live_mode() {
            let (subscriber, captured_spans, _events) = setup_capture();
            let journal = Arc::new(InMemoryJournalStorage::new());
            let provider = FakeProvider::new(vec![text_response("hello from live mode")]);
            let mut agent = build_loop(
                provider,
                ToolRegistry::new(),
                Box::new(PassthroughContext),
                journal,
                default_budget(),
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = agent.run("capture live journal spans").await.unwrap();

            let spans = captured_spans.lock().unwrap();
            let journal_span = spans
                .iter()
                .find(|span| {
                    span.fields.get("simulacra.operation.name")
                        == Some(&"journal_append".to_string())
                        && span.fields.get("simulacra.journal.entry_kind")
                            == Some(&"TurnStart".to_string())
                })
                .expect("expected a journal_append span for a TurnStart entry");

            assert_eq!(
                journal_span.fields.get("simulacra.journal.mode"),
                Some(&"live".to_string()),
                "live journal appends should be tagged with simulacra.journal.mode=live"
            );
        }

        #[tokio::test]
        async fn replayed_journal_entries_are_tagged_replayed() {
            struct PanickingProvider;

            impl Provider for PanickingProvider {
                fn chat<'a>(
                    &'a self,
                    _messages: &'a [Message],
                    _tools: &'a [ToolDefinition],
                    _budget: &'a mut ResourceBudget,
                ) -> std::pin::Pin<
                    Box<
                        dyn std::future::Future<
                                Output = Result<ProviderResponse, simulacra_types::ProviderError>,
                            > + Send
                            + 'a,
                    >,
                > {
                    panic!("Provider::chat should not be called during replay");
                }
            }

            let (subscriber, captured_spans, _events) = setup_capture();
            let journal = Arc::new(InMemoryJournalStorage::new());
            let replay_entries = vec![
                JournalEntry {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    agent_id: AgentId("test-agent".into()),
                    timestamp_ms: 1000,
                    entry: JournalEntryKind::TurnStart,
                },
                JournalEntry {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    agent_id: AgentId("test-agent".into()),
                    timestamp_ms: 1001,
                    entry: JournalEntryKind::LlmRequest {
                        model: "test-model".into(),
                        message_count: 2,
                    },
                },
                JournalEntry {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    agent_id: AgentId("test-agent".into()),
                    timestamp_ms: 1002,
                    entry: JournalEntryKind::LlmResponse {
                        model: "test-model".into(),
                        token_usage: TokenUsage {
                            input_tokens: 10,
                            output_tokens: 5,
                        },
                        finish_reason: "EndTurn".into(),
                        assistant_message: Some(Message {
                            role: Role::Assistant,
                            content: "replayed answer".into(),
                            tool_calls: vec![],
                            tool_call_id: None,
                        }),
                    },
                },
            ];

            let mut agent = AgentLoop::with_clock_and_replay(
                default_config(),
                Box::new(PanickingProvider),
                ToolRegistry::new(),
                Box::new(PassthroughContext),
                journal,
                default_budget(),
                Box::new(FixedClock(2_000)),
                Some(replay_entries),
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = agent.run("replayed task").await.unwrap();

            let spans = captured_spans.lock().unwrap();
            assert!(
                spans.iter().any(|span| {
                    span.fields.get("simulacra.operation.name")
                        == Some(&"journal_append".to_string())
                        && span.fields.get("simulacra.journal.mode")
                            == Some(&"replayed".to_string())
                }),
                "expected replayed journal appends to be tagged with simulacra.journal.mode=replayed"
            );
        }

        #[tokio::test]
        async fn journal_entries_counter_tracks_entries_by_kind() {
            let (subscriber, _spans, captured_events) = setup_capture();
            let journal = Arc::new(InMemoryJournalStorage::new());
            let provider = FakeProvider::new(vec![text_response("count journal entries")]);
            let mut agent = build_loop(
                provider,
                ToolRegistry::new(),
                Box::new(PassthroughContext),
                journal,
                default_budget(),
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = agent.run("emit journal metrics").await.unwrap();

            let events = captured_events.lock().unwrap();
            assert!(
                events.iter().any(|event| {
                    event.fields.get("simulacra.journal.entries") == Some(&"1".to_string())
                        && event.fields.get("simulacra.journal.entry_kind")
                            == Some(&"TurnStart".to_string())
                }),
                "expected simulacra.journal.entries counter updates tagged by entry kind"
            );
        }

        #[tokio::test]
        async fn journal_replay_ratio_gauge_reports_fraction_replayed() {
            struct PanickingProvider;

            impl Provider for PanickingProvider {
                fn chat<'a>(
                    &'a self,
                    _messages: &'a [Message],
                    _tools: &'a [ToolDefinition],
                    _budget: &'a mut ResourceBudget,
                ) -> std::pin::Pin<
                    Box<
                        dyn std::future::Future<
                                Output = Result<ProviderResponse, simulacra_types::ProviderError>,
                            > + Send
                            + 'a,
                    >,
                > {
                    panic!("Provider::chat should not be called during replay");
                }
            }

            let (subscriber, _spans, captured_events) = setup_capture();
            let journal = Arc::new(InMemoryJournalStorage::new());
            let replay_entries = vec![
                JournalEntry {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    agent_id: AgentId("test-agent".into()),
                    timestamp_ms: 1000,
                    entry: JournalEntryKind::TurnStart,
                },
                JournalEntry {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    agent_id: AgentId("test-agent".into()),
                    timestamp_ms: 1001,
                    entry: JournalEntryKind::LlmRequest {
                        model: "test-model".into(),
                        message_count: 2,
                    },
                },
                JournalEntry {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    agent_id: AgentId("test-agent".into()),
                    timestamp_ms: 1002,
                    entry: JournalEntryKind::LlmResponse {
                        model: "test-model".into(),
                        token_usage: TokenUsage {
                            input_tokens: 10,
                            output_tokens: 5,
                        },
                        finish_reason: "EndTurn".into(),
                        assistant_message: Some(Message {
                            role: Role::Assistant,
                            content: "fully replayed".into(),
                            tool_calls: vec![],
                            tool_call_id: None,
                        }),
                    },
                },
            ];

            let mut agent = AgentLoop::with_clock_and_replay(
                default_config(),
                Box::new(PanickingProvider),
                ToolRegistry::new(),
                Box::new(PassthroughContext),
                journal,
                default_budget(),
                Box::new(FixedClock(2_100)),
                Some(replay_entries),
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = agent.run("measure replay ratio").await.unwrap();

            let events = captured_events.lock().unwrap();
            assert!(
                events.iter().any(|event| {
                    event
                        .fields
                        .get("simulacra.journal.replay.ratio")
                        .is_some_and(|value| value == "1" || value == "1.0")
                }),
                "expected simulacra.journal.replay.ratio gauge to report the replay fraction"
            );
        }

        #[tokio::test]
        async fn capability_denials_emit_warn_event_on_current_span() {
            let (subscriber, captured_spans, captured_events) = setup_capture();
            let journal = Arc::new(InMemoryJournalStorage::new());
            let provider = FakeProvider::new(vec![
                tool_call_response("deny_shell", serde_json::json!({})),
                text_response("done"),
            ]);
            let mut tools = ToolRegistry::new();
            tools.register(Box::new(DenyShellTool));

            let mut agent = build_loop(
                provider,
                tools,
                Box::new(PassthroughContext),
                journal,
                default_budget(),
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = agent.run("emit a capability denial").await.unwrap();

            let spans = captured_spans.lock().unwrap();
            let agent_span = spans
                .iter()
                .find(|span| {
                    span.name == "invoke_agent"
                        && span.fields.get("gen_ai.agent.name") == Some(&"test-agent".to_string())
                })
                .expect("expected an invoke_agent span with the agent name");

            let events = captured_events.lock().unwrap();
            let denial_event = events
                .iter()
                .find(|event| {
                    event.level == "WARN"
                        && event.current_span.as_deref() == Some("invoke_agent")
                        && event.fields.get("simulacra.capability.operation")
                            == Some(&"shell".to_string())
                        && event.fields.get("simulacra.capability.reason")
                            == Some(&"shell capability not granted".to_string())
                })
                .expect("expected a WARN capability denial event on the invoke_agent span");

            assert_eq!(
                agent_span.fields.get("gen_ai.agent.name"),
                Some(&"test-agent".to_string())
            );
            assert_eq!(
                denial_event.fields.get("simulacra.capability.denials"),
                Some(&"1".to_string()),
                "capability denials should increment the simulacra.capability.denials counter"
            );
        }

        #[tokio::test]
        async fn capability_denial_warn_event_includes_agent_name_for_attribution() {
            let (subscriber, _captured_spans, captured_events) = setup_capture();
            let journal = Arc::new(InMemoryJournalStorage::new());
            let provider = FakeProvider::new(vec![
                tool_call_response("deny_shell", serde_json::json!({})),
                text_response("done"),
            ]);
            let mut tools = ToolRegistry::new();
            tools.register(Box::new(DenyShellTool));

            let mut agent = build_loop(
                provider,
                tools,
                Box::new(PassthroughContext),
                journal,
                default_budget(),
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = agent.run("emit a capability denial").await.unwrap();

            let events = captured_events.lock().unwrap();
            let denial_event = events
                .iter()
                .find(|event| {
                    event.level == "WARN"
                        && event.current_span.as_deref() == Some("invoke_agent")
                        && event.fields.get("simulacra.capability.operation")
                            == Some(&"shell".to_string())
                })
                .expect("expected a WARN capability denial event on the invoke_agent span");

            assert_eq!(
                denial_event.fields.get("gen_ai.agent.name"),
                Some(&"test-agent".to_string()),
                "capability denial WARN event should include the agent name for attribution"
            );
        }

        #[tokio::test]
        async fn budget_exhaustion_is_logged_at_warn_with_resource_usage_and_limit() {
            let (subscriber, _spans, captured_events) = setup_capture();
            let journal = Arc::new(InMemoryJournalStorage::new());
            let provider = FakeProvider::new(vec![]);

            let mut budget = default_budget();
            budget.max_turns = 1;
            budget.used_turns = 1;

            let mut agent = build_loop(
                provider,
                ToolRegistry::new(),
                Box::new(PassthroughContext),
                journal,
                budget,
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let result = agent.run("trip the turn budget").await;
            assert!(result.is_err(), "an exhausted budget should stop the loop");

            let events = captured_events.lock().unwrap();
            let exhaustion_event = events
                .iter()
                .find(|event| {
                    event.level == "WARN"
                        && event.fields.get("simulacra.agent.budget.resource")
                            == Some(&"turns".to_string())
                        && event.fields.get("simulacra.agent.budget.used") == Some(&"1".to_string())
                        && event.fields.get("simulacra.agent.budget.limit")
                            == Some(&"1".to_string())
                })
                .expect("expected a WARN event with the exhausted resource, used value, and limit");

            assert_eq!(
                exhaustion_event.current_span.as_deref(),
                Some("invoke_agent")
            );
        }

        #[tokio::test]
        async fn budget_remaining_gauge_is_updated_after_each_budget_consuming_operation() {
            let (subscriber, _spans, captured_events) = setup_capture();
            let journal = Arc::new(InMemoryJournalStorage::new());
            let provider = FakeProvider::new(vec![
                tool_call_response("echo", serde_json::json!({"msg": "hi"})),
                text_response("Done!"),
            ]);
            let mut tools = ToolRegistry::new();
            tools.register(Box::new(EchoTool));

            let mut agent = build_loop(
                provider,
                tools,
                Box::new(PassthroughContext),
                journal,
                default_budget(),
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let _output = agent
                .run("consume budget twice")
                .await
                .expect("run should succeed");

            let events = captured_events.lock().unwrap();
            let gauge_updates = events
                .iter()
                .filter(|event| {
                    event
                        .fields
                        .contains_key("simulacra.agent.budget.remaining")
                })
                .count();

            assert!(
                gauge_updates >= 2,
                "expected simulacra.agent.budget.remaining to be updated after each budget-consuming operation"
            );
        }

        #[tokio::test]
        async fn budget_check_failures_emit_current_span_event_with_exhaustion_details() {
            let (subscriber, captured_spans, captured_events) = setup_capture();
            let journal = Arc::new(InMemoryJournalStorage::new());
            let provider = FakeProvider::new(vec![]);

            let mut budget = default_budget();
            budget.max_turns = 1;
            budget.used_turns = 1;

            let mut agent = build_loop(
                provider,
                ToolRegistry::new(),
                Box::new(PassthroughContext),
                journal,
                budget,
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let result = agent.run("emit a budget exhaustion event").await;
            assert!(result.is_err(), "an exhausted budget should fail the run");

            let spans = captured_spans.lock().unwrap();
            assert!(
                spans.iter().any(|span| {
                    span.name == "invoke_agent"
                        && span.fields.get("gen_ai.agent.name") == Some(&"test-agent".to_string())
                }),
                "expected the invoke_agent span to be active while the budget failure is emitted"
            );

            let events = captured_events.lock().unwrap();
            let exhaustion_event = events
                .iter()
                .find(|event| {
                    event.current_span.as_deref() == Some("invoke_agent")
                        && event
                            .fields
                            .get("message")
                            .is_some_and(|message| message.contains("budget exhausted"))
                        && event.fields.get("simulacra.agent.budget.resource")
                            == Some(&"turns".to_string())
                        && event.fields.get("simulacra.agent.budget.used") == Some(&"1".to_string())
                        && event.fields.get("simulacra.agent.budget.limit")
                            == Some(&"1".to_string())
                })
                .expect("expected a current-span budget exhaustion event with detailed fields");

            assert_eq!(
                exhaustion_event.current_span.as_deref(),
                Some("invoke_agent")
            );
        }

        #[tokio::test]
        async fn replay_divergence_is_logged_at_error() {
            struct PanickingProvider;

            impl Provider for PanickingProvider {
                fn chat<'a>(
                    &'a self,
                    _messages: &'a [Message],
                    _tools: &'a [ToolDefinition],
                    _budget: &'a mut ResourceBudget,
                ) -> std::pin::Pin<
                    Box<
                        dyn std::future::Future<
                                Output = Result<ProviderResponse, simulacra_types::ProviderError>,
                            > + Send
                            + 'a,
                    >,
                > {
                    panic!("Provider::chat should not be called for a divergence test");
                }
            }

            let (subscriber, _spans, captured_events) = setup_capture();
            let journal = Arc::new(InMemoryJournalStorage::new());
            let replay_entries = vec![
                JournalEntry {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    agent_id: AgentId("test-agent".into()),
                    timestamp_ms: 1000,
                    entry: JournalEntryKind::TurnStart,
                },
                JournalEntry {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    agent_id: AgentId("test-agent".into()),
                    timestamp_ms: 1001,
                    entry: JournalEntryKind::LlmRequest {
                        model: "test-model".into(),
                        message_count: 2,
                    },
                },
                JournalEntry {
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    agent_id: AgentId("test-agent".into()),
                    timestamp_ms: 1002,
                    entry: JournalEntryKind::ToolResult {
                        tool_call_id: Some("tc-1".into()),
                        tool_name: "echo".into(),
                        content: "wrong entry kind".into(),
                        is_error: false,
                    },
                },
            ];

            let mut agent = AgentLoop::with_clock_and_replay(
                default_config(),
                Box::new(PanickingProvider),
                ToolRegistry::new(),
                Box::new(PassthroughContext),
                journal,
                default_budget(),
                Box::new(FixedClock(2_200)),
                Some(replay_entries),
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let result = agent.run("hit replay divergence").await;
            assert!(result.is_err(), "divergent replay should fail loudly");

            let events = captured_events.lock().unwrap();
            let divergence_event = events
                .iter()
                .find(|event| {
                    event.level == "ERROR"
                        && event
                            .fields
                            .values()
                            .any(|value| value.contains("LlmResponse"))
                        && event
                            .fields
                            .values()
                            .any(|value| value.contains("ToolResult"))
                })
                .expect("expected an ERROR log describing the replay divergence");

            assert_eq!(divergence_event.level, "ERROR");
        }

        #[tokio::test]
        async fn invoke_agent_span_contains_child_llm_span() {
            // Edge case: the invoke_agent span should wrap the full run so provider chat spans are
            // nested underneath it instead of being emitted as detached top-level spans.
            #[derive(Debug, Clone)]
            struct SpanRelationship {
                name: String,
                parent: Option<String>,
            }

            struct ParentCaptureLayer {
                spans: Arc<StdMutex<Vec<SpanRelationship>>>,
            }

            impl<S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>
                tracing_subscriber::Layer<S> for ParentCaptureLayer
            {
                fn on_new_span(
                    &self,
                    attrs: &tracing::span::Attributes<'_>,
                    _id: &tracing::span::Id,
                    ctx: tracing_subscriber::layer::Context<'_, S>,
                ) {
                    let parent = attrs
                        .parent()
                        .and_then(|parent_id| ctx.span(parent_id))
                        .map(|span| span.name().to_string())
                        .or_else(|| {
                            if attrs.is_contextual() {
                                ctx.current_span()
                                    .id()
                                    .and_then(|parent_id| ctx.span(parent_id))
                                    .map(|span| span.name().to_string())
                            } else {
                                None
                            }
                        });

                    self.spans.lock().unwrap().push(SpanRelationship {
                        name: attrs.metadata().name().to_string(),
                        parent,
                    });
                }
            }

            struct InstrumentedProvider;

            impl Provider for InstrumentedProvider {
                fn chat<'a>(
                    &'a self,
                    _messages: &'a [Message],
                    _tools: &'a [ToolDefinition],
                    _budget: &'a mut ResourceBudget,
                ) -> std::pin::Pin<
                    Box<
                        dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>>
                            + Send
                            + 'a,
                    >,
                > {
                    Box::pin(async move {
                        let _chat_span = tracing::info_span!(
                            "chat",
                            "gen_ai.operation.name" = "chat",
                            "gen_ai.request.model" = "instrumented-test-model",
                            "gen_ai.provider.name" = "fake",
                        )
                        .entered();
                        Ok(text_response("nested span"))
                    })
                }
            }

            let spans = Arc::new(StdMutex::new(Vec::new()));
            let subscriber =
                tracing_subscriber::registry::Registry::default().with(ParentCaptureLayer {
                    spans: Arc::clone(&spans),
                });
            let journal = Arc::new(InMemoryJournalStorage::new());
            let mut agent = AgentLoop::new(
                default_config(),
                Box::new(InstrumentedProvider),
                ToolRegistry::new(),
                Box::new(PassthroughContext),
                journal,
                default_budget(),
                None,
                None,
            );

            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = agent.run("nest spans").await.unwrap();

            let spans = spans.lock().unwrap();
            assert!(spans.iter().any(|span| span.name == "invoke_agent"));
            assert!(spans.iter().any(|span| {
                span.name == "chat" && span.parent.as_deref() == Some("invoke_agent")
            }));
        }
    }
}
