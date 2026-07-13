use super::*;

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
            tools: Arc::new(tools),
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
            cancellation: None,
            input_queue: None,
            hitl: None,
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
            tools: Arc::new(tools),
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
            cancellation: None,
            input_queue: None,
            hitl: None,
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
        self.refresh_budget_from_mirror();
    }

    /// Refresh the loop-owned working copy from the shared authoritative budget.
    ///
    /// The supervisor may roll child usage into the shared budget between parent
    /// turns. Refreshing before a new operation ensures those deductions constrain
    /// the parent's next budget check.
    pub(super) fn refresh_budget_from_mirror(&mut self) {
        if let Some(ref mirror) = self.budget_mirror {
            match mirror.lock() {
                Ok(budget) => {
                    self.budget = budget.clone();
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to refresh shared budget mirror");
                }
            }
        }
        self.sync_turn_mirror();
    }

    /// Merge usage consumed by one parent operation into shared state.
    ///
    /// Only the delta since `before` is added while holding the mutex. This
    /// preserves child usage recorded concurrently by the supervisor instead
    /// of replacing it with the loop's older snapshot.
    pub(super) fn merge_budget_into_mirror(&mut self, before: &ResourceBudget) {
        if let Some(ref mirror) = self.budget_mirror {
            match mirror.lock() {
                Ok(mut shared) => {
                    shared.used_tokens = shared
                        .used_tokens
                        .saturating_add(self.budget.used_tokens.saturating_sub(before.used_tokens));
                    shared.used_turns = shared
                        .used_turns
                        .saturating_add(self.budget.used_turns.saturating_sub(before.used_turns));
                    if self.budget.used_cost > before.used_cost {
                        shared.used_cost += self.budget.used_cost - before.used_cost;
                    }
                    shared.used_sub_agents = shared.used_sub_agents.saturating_add(
                        self.budget
                            .used_sub_agents
                            .saturating_sub(before.used_sub_agents),
                    );
                    shared.used_vfs_bytes = shared.used_vfs_bytes.saturating_add(
                        self.budget
                            .used_vfs_bytes
                            .saturating_sub(before.used_vfs_bytes),
                    );
                    shared.used_fuel = shared
                        .used_fuel
                        .saturating_add(self.budget.used_fuel.saturating_sub(before.used_fuel));
                    self.budget = shared.clone();
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to merge shared budget mirror");
                }
            }
        }
        self.sync_turn_mirror();
    }

    /// Replace shared state during explicit checkpoint restoration.
    pub(super) fn replace_budget_mirror(&self) {
        if let Some(ref mirror) = self.budget_mirror {
            match mirror.lock() {
                Ok(mut shared) => {
                    *shared = self.budget.clone();
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to restore shared budget mirror");
                }
            }
        }
        self.sync_turn_mirror();
    }

    fn sync_turn_mirror(&self) {
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

    /// Attach the runtime cancellation token observed by provider/tool dispatch.
    pub fn set_cancellation_token(&mut self, cancellation: crate::CancellationToken) {
        self.cancellation = Some(cancellation);
    }

    /// Attach a child steering input queue observed between model turns.
    pub fn set_input_queue(&mut self, input_queue: AgentInputQueue) {
        self.input_queue = Some(input_queue);
    }

    /// Attach human-in-the-loop channels used by server-launched resumable waits.
    pub fn set_hitl_runtime(&mut self, hitl: AgentHitlRuntime) {
        self.hitl = Some(hitl);
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
}
