use super::*;

#[cfg(not(feature = "spawn"))]
const FALLBACK_GENERIC_AGENT_SYSTEM_PROMPT: &str =
    "You are a helpful AI assistant running inside Simulacra.";

fn default_generic_agent_system_prompt() -> String {
    #[cfg(feature = "spawn")]
    {
        crate::spawn_tool::DEFAULT_SYSTEM_PROMPT.to_string()
    }
    #[cfg(not(feature = "spawn"))]
    {
        FALLBACK_GENERIC_AGENT_SYSTEM_PROMPT.to_string()
    }
}

pub(super) struct ChildResultContext {
    pub(super) agent_id: AgentId,
    pub(super) parent_id: AgentId,
    pub(super) agent_type: String,
    pub(super) parent_budget: Arc<Mutex<ResourceBudget>>,
    pub(super) journal: Option<Arc<dyn simulacra_types::JournalStorage>>,
    pub(super) activity_sink: Arc<dyn ActivitySink>,
    pub(super) spawn_start: Instant,
}

impl AgentSupervisor {
    pub(super) fn validate_spawn_request(&self, config: &SpawnConfig) -> Result<(), RuntimeError> {
        // Budget checks first: reject early if the child's budget request
        // exceeds the parent's remaining headroom. This ensures budget
        // enforcement even when spawn_types or capabilities would also reject.
        {
            let budget = lock_mutex(&self.parent_budget, "parent_budget");
            // max_sub_agents == 0 means unlimited (S006/S018). Only check
            // when the parent has a finite sub-agent limit.
            if budget.max_sub_agents > 0 && budget.used_sub_agents >= budget.max_sub_agents {
                return Err(RuntimeError::BudgetExhausted(
                    simulacra_types::BudgetExhausted {
                        resource: "sub_agents".into(),
                        used: budget.used_sub_agents.to_string(),
                        limit: budget.max_sub_agents.to_string(),
                    },
                ));
            }

            // max_tokens == 0 means unlimited (S006/S018). Only check
            // remaining token headroom when the parent has a finite limit.
            if budget.max_tokens > 0 {
                let parent_remaining_tokens = budget.max_tokens.saturating_sub(budget.used_tokens);
                if config.budget.max_tokens > parent_remaining_tokens {
                    return Err(RuntimeError::BudgetExhausted(
                        simulacra_types::BudgetExhausted {
                            resource: "tokens".into(),
                            used: budget.used_tokens.to_string(),
                            limit: budget.max_tokens.to_string(),
                        },
                    ));
                }
            }

            // max_turns == 0 means unlimited (S006/S018). Only check
            // remaining turn headroom when the parent has a finite limit.
            if budget.max_turns > 0 {
                let parent_remaining_turns = budget.max_turns.saturating_sub(budget.used_turns);
                if config.budget.max_turns > parent_remaining_turns {
                    return Err(RuntimeError::BudgetExhausted(
                        simulacra_types::BudgetExhausted {
                            resource: "turns".into(),
                            used: budget.used_turns.to_string(),
                            limit: budget.max_turns.to_string(),
                        },
                    ));
                }
            }

            // max_cost == 0 means unlimited (S006/S018). Only check
            // remaining cost headroom when the parent has a finite limit.
            if !budget.max_cost.is_zero() {
                let parent_remaining_cost = budget.max_cost - budget.used_cost;
                if config.budget.max_cost > parent_remaining_cost {
                    return Err(RuntimeError::BudgetExhausted(
                        simulacra_types::BudgetExhausted {
                            resource: "cost".into(),
                            used: budget.used_cost.to_string(),
                            limit: budget.max_cost.to_string(),
                        },
                    ));
                }
            }
        }

        if let Err(exhausted) = config.budget.check_budget() {
            return Err(RuntimeError::BudgetExhausted(exhausted));
        }

        // The parent must have the child's agent_type in its spawn_types.
        // An empty spawn_types list means "no restriction" (same convention as
        // max_sub_agents == 0 meaning unlimited).
        // Only enforced for named agent_type spawns; generic (None) agents
        // skip the spawn_types check.
        if let Some(ref at) = config.agent_type
            && !self.parent_capability.spawn_types.is_empty()
            && !self.parent_capability.spawn_types.contains(at)
        {
            return Err(RuntimeError::CapabilityViolation(format!(
                "agent_type {:?} is not in parent spawn_types {:?}",
                at, self.parent_capability.spawn_types
            )));
        }

        if let Some(ref cap) = config.capability
            && !cap.is_subset_of(&self.parent_capability)
        {
            return Err(RuntimeError::CapabilityViolation(
                "child capability is not a subset of parent capability".into(),
            ));
        }

        Ok(())
    }

    pub(super) fn prepare_spawn(&self, config: &SpawnConfig) -> Result<(), RuntimeError> {
        let agent_name = config.agent_id.0.as_str();
        let parent = config.parent_id.0.as_str();
        let capabilities = format!("{:?}", config.capability.as_ref());

        let child_agent_type = config.agent_type.as_deref().unwrap_or("generic");
        let spawn_mode = if config.agent_type.is_some() {
            "configured"
        } else {
            "generic"
        };
        let tier_label = config
            .resolved_tier
            .as_deref()
            .or(config.tier.as_deref())
            .unwrap_or("balanced");
        let _span = tracing::info_span!(
            "create_agent",
            "gen_ai.operation.name" = "create_agent",
            "gen_ai.agent.name" = agent_name,
            "simulacra.parent.agent_id" = parent,
            "simulacra.child.agent_type" = child_agent_type,
            "simulacra.agent.spawn_mode" = spawn_mode,
            "simulacra.agent.tier" = tier_label,
        )
        .entered();

        // S018: Journal SubAgentSpawned before child execution begins.
        // The child_id links the parent journal to the child's own journal
        // stream in JournalStorage.
        if let Some(ref journal) = self.journal_storage {
            let spawned_entry = simulacra_types::JournalEntry {
                schema_version: simulacra_types::JOURNAL_SCHEMA_VERSION,
                agent_id: config.parent_id.clone(),
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                entry: simulacra_types::JournalEntryKind::SubAgentSpawned {
                    child_id: config.agent_id.clone(),
                    agent_type: config
                        .agent_type
                        .clone()
                        .unwrap_or_else(|| "generic".to_string()),
                    system_prompt: if config.agent_type.is_none() {
                        Some(
                            config
                                .system_prompt
                                .clone()
                                .filter(|prompt| !prompt.is_empty())
                                .unwrap_or_else(default_generic_agent_system_prompt),
                        )
                    } else {
                        None
                    },
                },
            };
            journal
                .append(spawned_entry)
                .map_err(|source| RuntimeError::JournalAppendFailed {
                    entry_kind: "SubAgentSpawned",
                    source,
                })?;
        }

        lock_mutex(&self.parent_budget, "parent_budget").used_sub_agents += 1;

        tracing::info!(
            "gen_ai.agent.name" = agent_name,
            parent = parent,
            capabilities = capabilities.as_str(),
            "agent spawned"
        );

        // S019: Emit ActivityEvent::ChildSpawned before the child starts
        self.activity_sink.emit(ActivityEvent::ChildSpawned {
            child_id: config.agent_id.0.clone(),
            agent_type: config
                .agent_type
                .clone()
                .unwrap_or_else(|| "generic".to_string()),
            task: config.task.clone(),
        });

        Ok(())
    }

    /// Spawn a child agent under supervision.
    ///
    /// Validates:
    /// - Child CapabilityToken is_subset_of parent token (capability attenuation).
    /// - Child budget does not exceed parent budget (check_budget, used_sub_agents, max_sub_agents).
    /// - Emits a `create_agent` span with `gen_ai.operation.name` and `gen_ai.agent.name`.
    /// - Logs spawn at INFO with agent name, parent, and capabilities.
    pub fn spawn_agent(&mut self, config: SpawnConfig) -> Result<CancellationToken, RuntimeError> {
        self.validate_spawn_request(&config)?;

        // WARNING 1 fix: spawn_agent must have a task factory. Returning Ok(token)
        // without running any task was misleading — callers have no way to know
        // the spawn silently did nothing. This is a programmer error at wiring
        // time; fail fast instead of pretending success.
        let Some(factory) = self.task_factory.clone() else {
            return Err(RuntimeError::SpawnMissingTask);
        };
        factory.validate_spawn_config(&config)?;

        self.prepare_spawn(&config)?;

        let token = CancellationToken::new(Duration::from_secs(5));
        let agent_id = config.agent_id.clone();
        let agent_id_for_map = agent_id.clone();
        let started_at_ms = super::actor::now_ms();
        let spawn_start = Instant::now();
        let result_context = self.child_result_context(&config, spawn_start);
        let retry_config = config.clone();
        let (input_queue, input_handle) = AgentInputQueue::new();
        let mut task_future =
            factory.create_task_with_input(config.clone(), token.clone(), input_queue);
        lock_mutex(&self.cancellation_tokens, "cancellation_tokens")
            .insert(agent_id.clone(), token.clone());
        lock_mutex(&self.child_inputs, "child_inputs").insert(agent_id.clone(), input_handle);
        lock_mutex(&self.child_results, "child_results").insert(
            agent_id.clone(),
            ChildRunState {
                metadata: super::actor::child_metadata(&config, started_at_ms),
                result: None,
                join_waiters: Vec::new(),
                wait_waiters: Vec::new(),
            },
        );

        // Try polling the future once synchronously. If the task factory
        // resolves immediately (as in tests or simple delegation), we
        // handle the result on the caller's thread so tracing events are
        // emitted through the caller's subscriber.
        let immediate = {
            let waker = noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            Pin::as_mut(&mut task_future).poll(&mut cx)
        };

        if let std::task::Poll::Ready(result) = immediate {
            // WARNING 1 fix: if the child immediately errored, propagate that
            // error instead of returning Ok(token). We still call
            // `process_child_result` so journaling, activity events, and
            // tracing fire for the failure — the caller sees the error too.
            let was_err = result.is_err();
            // Clone the error for propagation before process_child_result consumes it.
            let err_for_return = match &result {
                Ok(_) => None,
                Err(e) => Some(RuntimeError::Session(format!(
                    "child {} (agent_type={}) failed immediately: {e}",
                    agent_id.0, result_context.agent_type
                ))),
            };
            let finalized = Self::process_child_result(result, &result_context);
            let terminal = ChildTerminalResult {
                child_id: result_context.agent_id.clone(),
                agent_type: result_context.agent_type.clone(),
                status: status_from_spawn_result(&finalized),
                elapsed_ms: result_context.spawn_start.elapsed().as_millis() as u64,
                tool_uses: finalized.as_ref().map(count_tool_uses).unwrap_or(0),
                result: finalized.map_err(|err| err.to_string()),
            };
            Self::record_child_terminal_result(&self.child_results, terminal);
            lock_mutex(&self.cancellation_tokens, "cancellation_tokens").remove(&agent_id);
            lock_mutex(&self.child_inputs, "child_inputs").remove(&agent_id);
            let handle: JoinHandle<()> = tokio::spawn(async {});
            lock_mutex(&self.children, "children").insert(agent_id_for_map, handle);
            if was_err && let Some(err) = err_for_return {
                return Err(err);
            }
            return Ok(token);
        }

        // Future is pending — spawn it as a background task.
        let dispatch = tracing::dispatcher::get_default(|d| d.clone());
        let retry_counts = Arc::clone(&self.retry_counts_shared);
        let child_results = Arc::clone(&self.child_results);
        let cancellation_tokens = Arc::clone(&self.cancellation_tokens);
        let child_inputs = Arc::clone(&self.child_inputs);
        let handle: JoinHandle<()> = tokio::spawn(async move {
            let _guard = tracing::dispatcher::set_default(&dispatch);
            let result = super::restart::run_task_with_retries(
                factory,
                retry_config,
                task_future,
                retry_counts,
            )
            .await;
            let result = Self::process_child_result(result, &result_context);
            let terminal = ChildTerminalResult {
                child_id: result_context.agent_id.clone(),
                agent_type: result_context.agent_type.clone(),
                status: status_from_spawn_result(&result),
                elapsed_ms: result_context.spawn_start.elapsed().as_millis() as u64,
                tool_uses: result.as_ref().map(count_tool_uses).unwrap_or(0),
                result: result.map_err(|err| err.to_string()),
            };
            Self::record_child_terminal_result(&child_results, terminal);
            lock_mutex(&cancellation_tokens, "cancellation_tokens")
                .remove(&result_context.agent_id);
            lock_mutex(&child_inputs, "child_inputs").remove(&result_context.agent_id);
        });
        lock_mutex(&self.children, "children").insert(agent_id_for_map, handle);

        Ok(token)
    }

    pub(super) fn child_result_context(
        &self,
        config: &SpawnConfig,
        spawn_start: Instant,
    ) -> ChildResultContext {
        ChildResultContext {
            agent_id: config.agent_id.clone(),
            parent_id: config.parent_id.clone(),
            agent_type: config
                .agent_type
                .clone()
                .unwrap_or_else(|| "generic".to_string()),
            parent_budget: Arc::clone(&self.parent_budget),
            journal: self.journal_storage.clone(),
            activity_sink: Arc::clone(&self.activity_sink),
            spawn_start,
        }
    }

    /// Process a child task result: roll up budget, journal, emit tracing and
    /// S019 ActivityEvent::ChildFinished with aggregated stats (tool_uses, token_count, duration_ms).
    pub(super) fn process_child_result(
        result: Result<AgentLoopOutput, RuntimeError>,
        context: &ChildResultContext,
    ) -> SpawnResult {
        match result {
            Ok(output) => {
                let token_total = output.token_usage.total();
                let tool_uses = output.used_turns;
                let token_count = token_total;
                let duration_ms = context.spawn_start.elapsed().as_millis() as u64;

                let mut budget = lock_mutex(&context.parent_budget, "parent_budget");
                budget.used_tokens +=
                    output.token_usage.input_tokens + output.token_usage.output_tokens;
                budget.used_turns += output.used_turns;
                budget.used_cost += output.used_cost;
                drop(budget);

                // S018: Journal SubAgentCompleted { success: true }
                if let Some(j) = &context.journal {
                    let completed_entry = simulacra_types::JournalEntry {
                        schema_version: simulacra_types::JOURNAL_SCHEMA_VERSION,
                        agent_id: context.parent_id.clone(),
                        timestamp_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0),
                        entry: simulacra_types::JournalEntryKind::SubAgentCompleted {
                            child_id: context.agent_id.clone(),
                            success: true,
                        },
                    };
                    j.append(completed_entry).map_err(|source| {
                        RuntimeError::JournalAppendFailed {
                            entry_kind: "SubAgentCompleted",
                            source,
                        }
                    })?;
                }

                let exit_reason_str = exit_reason_to_snake_case(&output.exit_reason);

                // S019: Emit ActivityEvent::ChildFinished with aggregated stats
                context.activity_sink.emit(ActivityEvent::ChildFinished {
                    child_id: context.agent_id.0.clone(),
                    agent_type: context.agent_type.clone(),
                    exit_reason: exit_reason_str.clone(),
                    duration_ms,
                    tool_uses,
                    token_count,
                });

                tracing::info!(
                    child_id = context.agent_id.0.as_str(),
                    parent_id = context.parent_id.0.as_str(),
                    exit_reason = exit_reason_str.as_str(),
                    token_total = token_total,
                    "child agent completed"
                );
                Ok(output)
            }
            Err(err) => {
                let duration_ms = context.spawn_start.elapsed().as_millis() as u64;

                // S018: Journal SubAgentCompleted { success: false }
                if let Some(j) = &context.journal {
                    let failed_entry = simulacra_types::JournalEntry {
                        schema_version: simulacra_types::JOURNAL_SCHEMA_VERSION,
                        agent_id: context.parent_id.clone(),
                        timestamp_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0),
                        entry: simulacra_types::JournalEntryKind::SubAgentCompleted {
                            child_id: context.agent_id.clone(),
                            success: false,
                        },
                    };
                    j.append(failed_entry)
                        .map_err(|source| RuntimeError::JournalAppendFailed {
                            entry_kind: "SubAgentCompleted",
                            source,
                        })?;
                }

                // S019: Emit ChildFinished on failure too
                context.activity_sink.emit(ActivityEvent::ChildFinished {
                    child_id: context.agent_id.0.clone(),
                    agent_type: context.agent_type.clone(),
                    exit_reason: format!("Error: {err}"),
                    duration_ms,
                    tool_uses: 0,
                    token_count: 0,
                });

                tracing::warn!(
                    child_id = context.agent_id.0.as_str(),
                    parent_id = context.parent_id.0.as_str(),
                    agent_type = context.agent_type.as_str(),
                    failure_reason = %err,
                    "child agent failed"
                );
                Err(err)
            }
        }
    }
}

pub(super) fn count_tool_uses(output: &AgentLoopOutput) -> u64 {
    if let Some(reported) = output.reported_tool_uses {
        return reported;
    }
    // Tool-result messages are the structured child-output record of tool invocations.
    output
        .messages
        .iter()
        .filter(|message| message.role == simulacra_types::Role::Tool)
        .count() as u64
}

pub(super) fn status_from_spawn_result(result: &SpawnResult) -> String {
    match result {
        Ok(output) if output.exit_reason == simulacra_types::ExitReason::Cancelled => {
            "cancelled".to_string()
        }
        Ok(output) if matches!(output.exit_reason, simulacra_types::ExitReason::Error(_)) => {
            "failed".to_string()
        }
        Ok(_) => "completed".to_string(),
        Err(_) => "failed".to_string(),
    }
}

/// Create a no-op Waker for synchronous future polling.
fn noop_waker() -> std::task::Waker {
    fn noop_clone(_: *const ()) -> std::task::RawWaker {
        std::task::RawWaker::new(std::ptr::null(), &VTABLE)
    }
    fn noop(_: *const ()) {}
    static VTABLE: std::task::RawWakerVTable =
        std::task::RawWakerVTable::new(noop_clone, noop, noop, noop);
    // SAFETY: The vtable functions are valid no-ops and the data pointer is null.
    unsafe { std::task::Waker::from_raw(std::task::RawWaker::new(std::ptr::null(), &VTABLE)) }
}
