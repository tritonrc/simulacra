use super::*;
use tracing::instrument::WithSubscriber;

pub(super) struct ChildResultContext {
    pub(super) agent_id: AgentId,
    pub(super) parent_id: AgentId,
    pub(super) placement: String,
    pub(super) parent_budget: Arc<Mutex<ResourceBudget>>,
    pub(super) journal: Option<Arc<dyn simulacra_types::JournalStorage>>,
    pub(super) activity_sink: Arc<dyn ActivitySink>,
    pub(super) spawn_start: Instant,
}

pub(super) struct AcceptedBudgetReservation {
    pub(super) child_budget: Arc<Mutex<ResourceBudget>>,
    pub(super) parent_budget: Arc<Mutex<ResourceBudget>>,
}

/// Resolves the terminal event's route at emission time. Child environments
/// are constructed asynchronously after a spawn is accepted, so resolving at
/// `ChildResultContext` construction would race the registration of the
/// child's own forwarding sink and flatten a descendant terminal event.
struct TerminalActivitySink {
    router: Arc<dyn ActivitySink>,
    child_id: AgentId,
    parent_id: AgentId,
}

impl ActivitySink for TerminalActivitySink {
    fn emit(&self, event: ActivityEvent) {
        let parent_sink = self.router.immediate_parent_sink(&self.parent_id);
        let sink = if parent_sink.is_some() {
            self.router
                .immediate_parent_sink(&self.child_id)
                .or(parent_sink)
                .unwrap_or_else(|| Arc::clone(&self.router))
        } else {
            Arc::clone(&self.router)
        };
        sink.emit(event);
    }
}

fn validate_supervisor_limit<T>(
    field: &str,
    requested: T,
    parent_maximum: T,
    parent_remaining: T,
) -> Result<(), RuntimeError>
where
    T: Copy + Default + PartialEq + PartialOrd + std::fmt::Display,
{
    if requested == T::default() && parent_maximum != T::default() {
        return Err(RuntimeError::BudgetExhausted(
            simulacra_types::BudgetExhausted {
                resource: format!(
                    "{field} requested {requested} (unlimited), but immediate parent limit {parent_maximum} is finite"
                ),
                used: requested.to_string(),
                limit: parent_maximum.to_string(),
            },
        ));
    }
    if requested != T::default() && parent_maximum != T::default() && requested > parent_remaining {
        return Err(RuntimeError::BudgetExhausted(
            simulacra_types::BudgetExhausted {
                resource: format!(
                    "{field} requested {requested} exceeds immediate parent remaining {parent_remaining} (limit {parent_maximum})"
                ),
                used: requested.to_string(),
                limit: parent_remaining.to_string(),
            },
        ));
    }
    Ok(())
}

impl AgentSupervisor {
    /// Fail closed until the embedding explicitly establishes the root
    /// identity. In particular, a model-visible request must never gain root
    /// authority merely by being the first caller seen by a supervisor.
    pub(super) fn require_bound_root(&self) -> Result<AgentId, RuntimeError> {
        lock_mutex(&self.root_agent_id, "root_agent_id")
            .clone()
            .ok_or_else(|| {
                RuntimeError::CapabilityViolation(
                    "supervisor root identity is unbound; bind the configured root agent id before exposing spawn or child-control operations".into(),
                )
            })
    }

    /// Reject an internal id that already belongs to an accepted child before
    /// any factory, hook, budget, journal, activity, or map effect occurs.
    pub(super) fn ensure_child_id_is_new(&self, child_id: &AgentId) -> Result<(), RuntimeError> {
        if lock_mutex(&self.accepted_child_ids, "accepted_child_ids").contains(child_id) {
            return Err(RuntimeError::CapabilityViolation(format!(
                "child_id {:?} was already accepted by this supervisor and cannot be reused",
                child_id.0
            )));
        }
        Ok(())
    }

    /// Mark an id only after the accepted-spawn preparation has succeeded.
    /// This is intentionally never undone by terminal settlement or close.
    pub(super) fn record_accepted_child_id(&self, child_id: &AgentId) {
        lock_mutex(&self.accepted_child_ids, "accepted_child_ids").insert(child_id.clone());
    }

    fn budget_account_for_caller(
        &self,
        caller_id: &AgentId,
    ) -> Result<Arc<Mutex<AgentBudgetAccount>>, RuntimeError> {
        let root_agent_id = self.require_bound_root()?;
        if &root_agent_id == caller_id {
            return Ok(Arc::clone(&self.root_budget_account));
        }
        lock_mutex(&self.child_budget_accounts, "child_budget_accounts")
            .get(caller_id)
            .cloned()
            .ok_or_else(|| {
                RuntimeError::CapabilityViolation(format!(
                    "unknown or unauthenticated budget owner {:?}",
                    caller_id.0
                ))
            })
    }

    pub(super) fn require_spawn_journal(&self) -> Result<(), RuntimeError> {
        if self.journal_storage.is_none() {
            return Err(RuntimeError::SpawnMissingJournal);
        }
        Ok(())
    }

    pub(super) fn capability_for_caller(
        &self,
        caller_id: &AgentId,
    ) -> Result<CapabilityToken, RuntimeError> {
        let root_agent_id = self.require_bound_root()?;
        if &root_agent_id == caller_id {
            return Ok(self.parent_capability.clone());
        }

        lock_mutex(&self.child_results, "child_results")
            .get(caller_id)
            .map(|state| state.metadata.capability.clone())
            .ok_or_else(|| {
                RuntimeError::CapabilityViolation(format!(
                    "unknown or unauthenticated caller {:?}; supervisor root is {:?}",
                    caller_id.0, root_agent_id.0
                ))
            })
    }

    pub(super) fn validate_caller_identity(&self, caller_id: &AgentId) -> Result<(), String> {
        self.capability_for_caller(caller_id)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(super) fn validate_spawn_authorization(
        &self,
        config: &SpawnConfig,
    ) -> Result<(), RuntimeError> {
        let caller_capability = self.capability_for_caller(&config.parent_id)?;
        // Authorization is checked before any reservation or other accepted
        // spawn effect. Empty placement authorization is deny-all.
        if !caller_capability
            .spawn_placements
            .contains(&config.placement)
        {
            let mut available_placements = caller_capability.spawn_placements.clone();
            available_placements.sort();
            available_placements.dedup();
            return Err(RuntimeError::CapabilityViolation(format!(
                "placement {:?} is not authorized for caller {:?}; available spawn_placements: {:?}",
                config.placement, config.parent_id.0, available_placements
            )));
        }

        if let Some(ref cap) = config.capability
            && !cap.is_subset_of(&caller_capability)
        {
            return Err(RuntimeError::CapabilityViolation(format!(
                "child capability is not a subset of caller {:?} capability",
                config.parent_id.0
            )));
        }

        Ok(())
    }

    pub(super) fn validate_spawn_budget(&self, config: &SpawnConfig) -> Result<(), RuntimeError> {
        let account = self.budget_account_for_caller(&config.parent_id)?;
        let account = lock_mutex(&account, "agent_budget_account");
        Self::validate_budget_against_account(&config.budget, &account)?;

        if let Err(exhausted) = config.budget.check_budget() {
            return Err(RuntimeError::BudgetExhausted(exhausted));
        }

        Ok(())
    }

    fn validate_budget_against_account(
        requested: &ResourceBudget,
        account: &AgentBudgetAccount,
    ) -> Result<(), RuntimeError> {
        let budget = lock_mutex(&account.budget, "agent_budget");
        validate_supervisor_limit(
            "max_tokens",
            requested.max_tokens,
            budget.max_tokens,
            budget
                .max_tokens
                .saturating_sub(budget.used_tokens.saturating_add(account.reserved_tokens)),
        )?;
        validate_supervisor_limit(
            "max_turns",
            requested.max_turns,
            budget.max_turns,
            budget
                .max_turns
                .saturating_sub(budget.used_turns.saturating_add(account.reserved_turns)),
        )?;
        let committed_cost = budget
            .used_cost
            .checked_add(account.reserved_cost)
            .unwrap_or(rust_decimal::Decimal::MAX)
            .min(budget.max_cost);
        validate_supervisor_limit(
            "max_cost",
            requested.max_cost,
            budget.max_cost,
            budget.max_cost - committed_cost,
        )?;
        validate_supervisor_limit(
            "max_sub_agents",
            requested.max_sub_agents,
            budget.max_sub_agents,
            budget.max_sub_agents.saturating_sub(budget.used_sub_agents),
        )?;
        Ok(())
    }

    pub(super) fn reserve_spawn_budget(
        &self,
        config: &SpawnConfig,
    ) -> Result<AcceptedBudgetReservation, RuntimeError> {
        let parent = self.budget_account_for_caller(&config.parent_id)?;
        let parent_budget = {
            let mut account = lock_mutex(&parent, "agent_budget_account");
            Self::validate_budget_against_account(&config.budget, &account)?;
            account.reserved_tokens = account
                .reserved_tokens
                .saturating_add(config.budget.max_tokens);
            account.reserved_turns = account
                .reserved_turns
                .saturating_add(config.budget.max_turns);
            account.reserved_cost = account
                .reserved_cost
                .checked_add(config.budget.max_cost)
                .unwrap_or(rust_decimal::Decimal::MAX);
            let budget = Arc::clone(&account.budget);
            {
                let mut budget_state = lock_mutex(&budget, "agent_budget");
                budget_state.used_sub_agents = budget_state.used_sub_agents.saturating_add(1);
            }
            budget
        };

        let child_budget = Arc::new(Mutex::new(config.budget.clone()));
        lock_mutex(&self.child_budget_accounts, "child_budget_accounts").insert(
            config.agent_id.clone(),
            Arc::new(Mutex::new(AgentBudgetAccount::new(Arc::clone(
                &child_budget,
            )))),
        );
        lock_mutex(&self.budget_reservations, "budget_reservations").insert(
            config.agent_id.clone(),
            BudgetReservation {
                parent,
                tokens: config.budget.max_tokens,
                turns: config.budget.max_turns,
                cost: config.budget.max_cost,
            },
        );
        Ok(AcceptedBudgetReservation {
            child_budget,
            parent_budget,
        })
    }

    pub(super) fn settle_budget_reservation(
        reservations: &Arc<Mutex<HashMap<AgentId, BudgetReservation>>>,
        child_id: &AgentId,
        result: &SpawnResult,
    ) {
        let reservation = lock_mutex(reservations, "budget_reservations").remove(child_id);
        if let Some(reservation) = reservation {
            let mut account = lock_mutex(&reservation.parent, "agent_budget_account");
            account.reserved_tokens = account.reserved_tokens.saturating_sub(reservation.tokens);
            account.reserved_turns = account.reserved_turns.saturating_sub(reservation.turns);
            account.reserved_cost = account
                .reserved_cost
                .checked_sub(reservation.cost)
                .unwrap_or(rust_decimal::Decimal::ZERO);
            if let Ok(output) = result {
                let mut budget = lock_mutex(&account.budget, "agent_budget");
                Self::charge_budget_usage(&mut budget, output);
            }
        }
    }

    fn charge_budget_usage(budget: &mut ResourceBudget, output: &AgentLoopOutput) {
        budget.used_tokens = budget
            .used_tokens
            .saturating_add(output.token_usage.total());
        budget.used_turns = budget.used_turns.saturating_add(output.used_turns);
        budget.used_cost += output.used_cost;
    }

    pub(super) fn rollback_unaccepted_budget(&self, child_id: &AgentId) {
        let reservation =
            lock_mutex(&self.budget_reservations, "budget_reservations").remove(child_id);
        if let Some(reservation) = reservation {
            let mut account = lock_mutex(&reservation.parent, "agent_budget_account");
            account.reserved_tokens = account.reserved_tokens.saturating_sub(reservation.tokens);
            account.reserved_turns = account.reserved_turns.saturating_sub(reservation.turns);
            account.reserved_cost = account
                .reserved_cost
                .checked_sub(reservation.cost)
                .unwrap_or(rust_decimal::Decimal::ZERO);
            let mut budget = lock_mutex(&account.budget, "agent_budget");
            budget.used_sub_agents = budget.used_sub_agents.saturating_sub(1);
        }
        lock_mutex(&self.child_budget_accounts, "child_budget_accounts").remove(child_id);
    }

    pub(super) fn prepare_spawn(
        &self,
        config: &SpawnConfig,
        backend: AgentBackend,
    ) -> Result<(), RuntimeError> {
        let agent_name = config.agent_id.0.as_str();
        let parent = config.parent_id.0.as_str();
        let child_placement = config.placement.as_str();
        let child_backend = match backend {
            AgentBackend::Native => "native",
            AgentBackend::Acp => "acp",
        };
        // S018: Journal SubAgentSpawned before child execution begins.
        // The child_id links the parent journal to the child's own journal
        // stream in JournalStorage.
        let journal = self
            .journal_storage
            .as_ref()
            .ok_or(RuntimeError::SpawnMissingJournal)?;
        let spawned_entry = simulacra_types::JournalEntry {
            schema_version: simulacra_types::JOURNAL_SCHEMA_VERSION,
            agent_id: config.parent_id.clone(),
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            entry: simulacra_types::JournalEntryKind::SubAgentSpawned {
                child_id: config.agent_id.clone(),
                placement: config.placement.clone(),
                backend: child_backend.to_string(),
                task: config.task.clone(),
                instructions: config.instructions.clone(),
            },
        };
        journal
            .append(spawned_entry)
            .map_err(|source| RuntimeError::JournalAppendFailed {
                entry_kind: "SubAgentSpawned",
                source,
            })?;

        tracing::info!(
            child_id = agent_name,
            parent_id = parent,
            placement = child_placement,
            backend = child_backend,
            instruction_length_bytes = config.instructions.as_ref().map_or(0, String::len),
            "agent spawned"
        );

        // S019: Emit ActivityEvent::ChildSpawned before the child starts
        let immediate_parent_sink = self
            .activity_sink
            .immediate_parent_sink(&config.parent_id)
            .unwrap_or_else(|| Arc::clone(&self.activity_sink));
        immediate_parent_sink.emit(ActivityEvent::ChildSpawned {
            child_id: config.agent_id.0.clone(),
            placement: config.placement.clone(),
            task: config.task.clone(),
        });

        Ok(())
    }

    pub(super) fn create_agent_span(
        config: &SpawnConfig,
        backend: AgentBackend,
        parent: Option<&tracing::Span>,
    ) -> tracing::Span {
        let child_backend = match backend {
            AgentBackend::Native => "native",
            AgentBackend::Acp => "acp",
        };
        if let Some(parent) = parent {
            tracing::info_span!(
                parent: parent,
                "create_agent",
                "gen_ai.operation.name" = "create_agent",
                "gen_ai.agent.name" = config.agent_id.0.as_str(),
                "simulacra.parent.agent_id" = config.parent_id.0.as_str(),
                "simulacra.child.placement" = config.placement.as_str(),
                "simulacra.child.backend" = child_backend,
            )
        } else {
            tracing::info_span!(
                "create_agent",
                "gen_ai.operation.name" = "create_agent",
                "gen_ai.agent.name" = config.agent_id.0.as_str(),
                "simulacra.parent.agent_id" = config.parent_id.0.as_str(),
                "simulacra.child.placement" = config.placement.as_str(),
                "simulacra.child.backend" = child_backend,
            )
        }
    }

    /// Spawn a child agent under supervision.
    ///
    /// Validates:
    /// - Child CapabilityToken is_subset_of parent token (capability attenuation).
    /// - Child budget does not exceed parent budget (check_budget, used_sub_agents, max_sub_agents).
    /// - Emits a `create_agent` span with `gen_ai.operation.name` and `gen_ai.agent.name`.
    /// - Logs spawn at INFO with agent name, parent, and capabilities.
    pub fn spawn_agent(
        &mut self,
        mut config: SpawnConfig,
    ) -> Result<CancellationToken, RuntimeError> {
        self.require_bound_root()?;
        self.ensure_child_id_is_new(&config.agent_id)?;
        self.validate_spawn_authorization(&config)?;

        // WARNING 1 fix: spawn_agent must have a task factory. Returning Ok(token)
        // without running any task was misleading — callers have no way to know
        // the spawn silently did nothing. This is a programmer error at wiring
        // time; fail fast instead of pretending success.
        let Some(factory) = self.task_factory.clone() else {
            // A missing factory cannot perform live placement resolution, but
            // the supervisor must still enforce the parent budget at its own
            // boundary before reporting the wiring error.
            self.validate_spawn_budget(&config)?;
            return Err(RuntimeError::SpawnMissingTask);
        };
        factory.validate_spawn_config(&config)?;
        self.require_spawn_journal()?;
        self.validate_spawn_budget(&config)?;
        let caller_capability = self.capability_for_caller(&config.parent_id)?;
        factory.prepare_spawn_config_for_caller(&mut config, &caller_capability)?;
        self.validate_spawn_authorization(&config)?;
        self.validate_spawn_budget(&config)?;

        let backend = factory.placement_backend(&config);
        let create_agent_span = Self::create_agent_span(&config, backend, None);
        let _create_agent_span = create_agent_span.entered();
        let accepted_budget = self.reserve_spawn_budget(&config)?;
        if let Err(error) = self.prepare_spawn(&config, backend) {
            self.rollback_unaccepted_budget(&config.agent_id);
            return Err(error);
        }
        self.record_accepted_child_id(&config.agent_id);

        let token = CancellationToken::new(Duration::from_secs(5));
        let agent_id = config.agent_id.clone();
        let agent_id_for_map = agent_id.clone();
        let started_at_ms = super::actor::now_ms();
        let spawn_start = Instant::now();
        let result_context =
            self.child_result_context(&config, spawn_start, accepted_budget.parent_budget);
        let retry_config = config.clone();
        let (input_queue, input_handle) = AgentInputQueue::new();
        let mut task_future = factory.create_task_with_input_and_budget(
            config.clone(),
            token.clone(),
            input_queue,
            accepted_budget.child_budget,
        );
        lock_mutex(&self.cancellation_tokens, "cancellation_tokens")
            .insert(agent_id.clone(), token.clone());
        lock_mutex(&self.child_inputs, "child_inputs").insert(agent_id.clone(), input_handle);
        lock_mutex(&self.child_results, "child_results").insert(
            agent_id.clone(),
            ChildRunState {
                metadata: super::actor::child_metadata(&config, started_at_ms),
                result: None,
                result_delivered: false,
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

        let task_future = if let std::task::Poll::Ready(result) = immediate {
            if spawn_result_is_awaiting_approval(&result) {
                let awaiting_cancellation = token.clone();
                Box::pin(async move {
                    await_awaiting_approval_terminal(result, awaiting_cancellation).await
                }) as BoxTaskFuture
            } else {
                // WARNING 1 fix: if the child immediately errored, propagate that
                // error instead of returning Ok(token). We still call
                // `process_child_result` so journaling, activity events, and
                // tracing fire for the failure — the caller sees the error too.
                let was_err = result.is_err();
                // Clone the error for propagation before process_child_result consumes it.
                let err_for_return = match &result {
                    Ok(_) => None,
                    Err(e) => Some(RuntimeError::Session(format!(
                        "child {} (placement={}) failed immediately: {e}",
                        agent_id.0, result_context.placement
                    ))),
                };
                let _after_hook_outcome = factory.after_spawn(&config, &result);
                Self::settle_budget_reservation(&self.budget_reservations, &agent_id, &result);
                let finalized =
                    Self::process_child_result_after_settlement(result, &result_context);
                let terminal = ChildTerminalResult {
                    child_id: result_context.agent_id.clone(),
                    placement: result_context.placement.clone(),
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
        } else {
            task_future
        };

        // AwaitingApproval is an intermediate compatibility state, not a
        // completed task. Keep the same accepted child lifecycle alive until
        // cancellation converts that state into an ordinary terminal result.
        // Placement backends with a live approval channel remain pending inside
        // their original task future and therefore never take this fallback.
        let lifecycle_cancellation = token.clone();

        // Future is pending — spawn it as a background task.
        let dispatch = tracing::dispatcher::get_default(|d| d.clone());
        let retry_counts = Arc::clone(&self.retry_counts_shared);
        let child_results = Arc::clone(&self.child_results);
        let cancellation_tokens = Arc::clone(&self.cancellation_tokens);
        let child_inputs = Arc::clone(&self.child_inputs);
        let budget_reservations = Arc::clone(&self.budget_reservations);
        let handle: JoinHandle<()> = tokio::spawn(
            async move {
                let result = super::restart::run_task_with_retries(
                    Arc::clone(&factory),
                    retry_config.clone(),
                    task_future,
                    retry_counts,
                )
                .await;
                let result = await_awaiting_approval_terminal(result, lifecycle_cancellation).await;
                let _after_hook_outcome = factory.after_spawn(&retry_config, &result);
                Self::settle_budget_reservation(
                    &budget_reservations,
                    &result_context.agent_id,
                    &result,
                );
                let result = Self::process_child_result_after_settlement(result, &result_context);
                let terminal = ChildTerminalResult {
                    child_id: result_context.agent_id.clone(),
                    placement: result_context.placement.clone(),
                    status: status_from_spawn_result(&result),
                    elapsed_ms: result_context.spawn_start.elapsed().as_millis() as u64,
                    tool_uses: result.as_ref().map(count_tool_uses).unwrap_or(0),
                    result: result.map_err(|err| err.to_string()),
                };
                Self::record_child_terminal_result(&child_results, terminal);
                lock_mutex(&cancellation_tokens, "cancellation_tokens")
                    .remove(&result_context.agent_id);
                lock_mutex(&child_inputs, "child_inputs").remove(&result_context.agent_id);
            }
            .with_subscriber(dispatch),
        );
        lock_mutex(&self.children, "children").insert(agent_id_for_map, handle);

        Ok(token)
    }

    pub(super) fn child_result_context(
        &self,
        config: &SpawnConfig,
        spawn_start: Instant,
        parent_budget: Arc<Mutex<ResourceBudget>>,
    ) -> ChildResultContext {
        ChildResultContext {
            agent_id: config.agent_id.clone(),
            parent_id: config.parent_id.clone(),
            placement: config.placement.clone(),
            parent_budget,
            journal: self.journal_storage.clone(),
            activity_sink: Arc::new(TerminalActivitySink {
                router: Arc::clone(&self.activity_sink),
                child_id: config.agent_id.clone(),
                parent_id: config.parent_id.clone(),
            }),
            spawn_start,
        }
    }

    /// Process a child task result: roll up budget, journal, emit tracing and
    /// S019 ActivityEvent::ChildFinished with aggregated stats (tool_uses, token_count, duration_ms).
    #[cfg(test)]
    pub(super) fn process_child_result(
        result: Result<AgentLoopOutput, RuntimeError>,
        context: &ChildResultContext,
    ) -> SpawnResult {
        Self::process_child_result_with_rollup(result, context, true)
    }

    pub(super) fn process_child_result_after_settlement(
        result: Result<AgentLoopOutput, RuntimeError>,
        context: &ChildResultContext,
    ) -> SpawnResult {
        Self::process_child_result_with_rollup(result, context, false)
    }

    fn process_child_result_with_rollup(
        result: Result<AgentLoopOutput, RuntimeError>,
        context: &ChildResultContext,
        rollup_usage: bool,
    ) -> SpawnResult {
        match result {
            Ok(output) => {
                let Some(success) = child_exit_success(&output.exit_reason) else {
                    return Ok(output);
                };
                let token_total = output.token_usage.total();
                let tool_uses = output.used_turns;
                let token_count = token_total;
                let duration_ms = context.spawn_start.elapsed().as_millis() as u64;

                if rollup_usage {
                    let mut budget = lock_mutex(&context.parent_budget, "parent_budget");
                    Self::charge_budget_usage(&mut budget, &output);
                }

                // S018: Journal the exhaustive terminal success mapping.
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
                            success,
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
                    placement: context.placement.clone(),
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
                    placement: context.placement.clone(),
                    exit_reason: format!("Error: {err}"),
                    duration_ms,
                    tool_uses: 0,
                    token_count: 0,
                });

                tracing::warn!(
                    child_id = context.agent_id.0.as_str(),
                    parent_id = context.parent_id.0.as_str(),
                    placement = context.placement.as_str(),
                    error_category = child_execution_error_category(&err),
                    "child agent failed"
                );
                Err(err)
            }
        }
    }
}

/// Maps child failures to S060's bounded, log-only error vocabulary.
///
/// This deliberately classifies by typed error boundary, never by rendered
/// error text. The original typed error is returned to the caller unchanged.
fn child_execution_error_category(error: &RuntimeError) -> &'static str {
    match error {
        RuntimeError::Provider(_) => "provider",
        RuntimeError::AcpChildRuntime { .. } => "acp_runtime",
        _ => "runtime",
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

pub(super) fn spawn_result_is_awaiting_approval(result: &SpawnResult) -> bool {
    matches!(
        result,
        Ok(output) if output.exit_reason == simulacra_types::ExitReason::AwaitingApproval
    )
}

pub(super) async fn await_awaiting_approval_terminal(
    mut result: SpawnResult,
    cancellation: CancellationToken,
) -> SpawnResult {
    if !spawn_result_is_awaiting_approval(&result) {
        return result;
    }

    while !cancellation.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    if let Ok(output) = &mut result {
        output.exit_reason = simulacra_types::ExitReason::Cancelled;
    }
    result
}

fn child_exit_success(exit_reason: &simulacra_types::ExitReason) -> Option<bool> {
    match exit_reason {
        simulacra_types::ExitReason::Complete
        | simulacra_types::ExitReason::MaxTurns
        | simulacra_types::ExitReason::BudgetExhausted => Some(true),
        simulacra_types::ExitReason::Error(_)
        | simulacra_types::ExitReason::GuardrailTripped(_)
        | simulacra_types::ExitReason::PolicyKill { .. }
        | simulacra_types::ExitReason::Cancelled => Some(false),
        simulacra_types::ExitReason::AwaitingApproval => None,
    }
}

pub(crate) fn status_from_spawn_result(result: &SpawnResult) -> String {
    match result {
        Ok(output) => match &output.exit_reason {
            simulacra_types::ExitReason::Complete
            | simulacra_types::ExitReason::MaxTurns
            | simulacra_types::ExitReason::BudgetExhausted => "completed".to_string(),
            simulacra_types::ExitReason::Error(_)
            | simulacra_types::ExitReason::GuardrailTripped(_)
            | simulacra_types::ExitReason::PolicyKill { .. } => "failed".to_string(),
            simulacra_types::ExitReason::Cancelled => "cancelled".to_string(),
            simulacra_types::ExitReason::AwaitingApproval => "running".to_string(),
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s059_child_budget_rollup_saturates_logical_usage_without_charging_cache_subsets() {
        let parent_budget = Arc::new(Mutex::new(ResourceBudget::new(
            0,
            10,
            rust_decimal::Decimal::new(100, 0),
            5,
        )));
        let context = ChildResultContext {
            agent_id: AgentId("extreme-child".into()),
            parent_id: AgentId("parent-agent".into()),
            placement: "test".into(),
            parent_budget: Arc::clone(&parent_budget),
            journal: None,
            activity_sink: Arc::new(NoopActivitySink),
            spawn_start: Instant::now(),
        };
        let output = AgentLoopOutput {
            exit_reason: simulacra_types::ExitReason::Complete,
            messages: vec![],
            token_usage: simulacra_types::TokenUsage {
                input_tokens: u64::MAX,
                output_tokens: 1,
                cache_read_input_tokens: u64::MAX - 7,
                cache_write_input_tokens: 7,
            },
            reported_tool_uses: None,
            used_turns: 1,
            used_cost: rust_decimal::Decimal::ZERO,
        };

        let completed = AgentSupervisor::process_child_result(Ok(output), &context)
            .expect("extreme child usage must roll up without panic or wrap");

        assert_eq!(
            lock_mutex(&parent_budget, "parent_budget").used_tokens,
            u64::MAX,
            "parent budget must saturate input plus output without adding cache subsets"
        );
        assert_eq!(completed.token_usage.cache_read_input_tokens, u64::MAX - 7);
        assert_eq!(completed.token_usage.cache_write_input_tokens, 7);
        assert_eq!(completed.token_usage.total(), u64::MAX);
    }

    #[test]
    fn s060_unmatched_terminal_never_fabricates_child_result_metadata_or_control_state() {
        let supervisor = AgentSupervisor::new(
            CapabilityToken::default(),
            ResourceBudget::new(0, 0, rust_decimal::Decimal::ZERO, 0),
        );
        let child_id = AgentId("never-accepted".into());

        AgentSupervisor::record_child_terminal_result(
            &supervisor.child_results,
            ChildTerminalResult {
                child_id: child_id.clone(),
                placement: "workspace".into(),
                status: "completed".into(),
                elapsed_ms: 1,
                tool_uses: 0,
                result: Err("unmatched terminal".into()),
            },
        );

        assert!(
            lock_mutex(&supervisor.child_results, "child_results").is_empty(),
            "an unmatched terminal must not fabricate cached result, metadata, or join state"
        );
        assert!(
            lock_mutex(&supervisor.cancellation_tokens, "cancellation_tokens").is_empty(),
            "an unmatched terminal must not create cancellation control state"
        );
        assert!(
            lock_mutex(&supervisor.child_inputs, "child_inputs").is_empty(),
            "an unmatched terminal must not create steering-input control state"
        );
        assert!(
            lock_mutex(&supervisor.children, "children").is_empty(),
            "an unmatched terminal must not create a live-child roster entry"
        );
        assert!(
            lock_mutex(&supervisor.accepted_child_ids, "accepted_child_ids").is_empty(),
            "an unmatched terminal must not reserve the opaque child id"
        );

        let (join_tx, mut join_rx) = tokio::sync::oneshot::channel();
        supervisor.join_child(child_id.clone(), join_tx);
        assert!(
            matches!(join_rx.try_recv(), Ok(Err(error)) if error.contains("unknown child_id")),
            "an unmatched terminal must not leave a joinable terminal result"
        );

        let (roster_tx, mut roster_rx) = tokio::sync::oneshot::channel();
        supervisor.send_child_roster_inspection(roster_tx);
        assert!(
            matches!(roster_rx.try_recv(), Ok(Ok(entries)) if entries.is_empty()),
            "an unmatched terminal must not appear in the host roster"
        );
    }
}
