use super::*;

impl AgentSupervisor {
    /// Apply a restart strategy after an agent failure.
    ///
    /// Tracks retry counts per agent. Returns true if the agent should be
    /// restarted, false if the failure should propagate.
    ///
    /// For `RestartStrategy::SnapshotAndFail`, the caller should invoke
    /// `save_checkpoint(` on the journal to persist a `Checkpoint` before
    /// propagating.
    ///
    /// Logs the restart at WARN with agent name, strategy, and failure reason.
    pub fn handle_failure(
        &self,
        agent_id: &AgentId,
        strategy: &RestartStrategy,
        failure_reason: &str,
    ) -> bool {
        let strategy_name = match strategy {
            RestartStrategy::RetryOnce => "retry_once",
            RestartStrategy::RetryTwiceThenFail => "retry_twice_then_fail",
            RestartStrategy::SnapshotAndFail => "snapshot_and_fail",
            RestartStrategy::LetCrash => "let_crash",
        };

        let should_restart = match strategy {
            RestartStrategy::RetryOnce => {
                let mut counts = self.retry_counts.lock().unwrap();
                let count = counts.entry(agent_id.clone()).or_insert(0);
                if *count < 1 {
                    *count += 1;
                    true
                } else {
                    false
                }
            }
            RestartStrategy::RetryTwiceThenFail => {
                let mut counts = self.retry_counts.lock().unwrap();
                let count = counts.entry(agent_id.clone()).or_insert(0);
                if *count < 2 {
                    *count += 1;
                    true
                } else {
                    false
                }
            }
            RestartStrategy::SnapshotAndFail => {
                self.snapshot_before_fail(agent_id, failure_reason);
                false
            }
            RestartStrategy::LetCrash => false,
        };

        if should_restart {
            tracing::warn!(
                "gen_ai.agent.name" = agent_id.0.as_str(),
                strategy = strategy_name,
                failure_reason = failure_reason,
                "agent restart triggered"
            );
        }

        should_restart
    }

    /// Save a journal Checkpoint before propagating a SnapshotAndFail failure.
    ///
    /// Behaves the same as `LetCrash` when no journal is wired. When a journal
    /// is available, persists `CheckpointData` so fork-from-checkpoint recovery
    /// is possible. Messages and vfs_snapshot are empty here because the
    /// supervisor does not hold conversational state directly — the journal
    /// already contains the full history up to this point.
    fn snapshot_before_fail(&self, agent_id: &AgentId, failure_reason: &str) {
        let checkpoint_data = simulacra_types::CheckpointData {
            messages: vec![],
            budget_snapshot: self.parent_budget.lock().unwrap().clone(),
            vfs_snapshot: None,
        };

        // WARNING 2 fix: this path was previously logging "checkpoint saved"
        // while not actually calling `save_checkpoint`. That silently claimed
        // a durable snapshot that didn't exist. We now attempt a real persist
        // when a journal is wired, and log a clear "NOT persisted" message
        // otherwise so operators aren't misled.
        if let Some(ref journal) = self.journal_storage {
            // Serialize the checkpoint into the `snapshot_data` payload and
            // append a Checkpoint journal entry (save_checkpoint writes the
            // entry under the agent's journal so it is recoverable via
            // `JournalStorage::latest_checkpoint` / `fork_from`).
            //
            // We use index 0 as a sentinel here because the supervisor does
            // not track a per-agent turn counter. When the supervisor is
            // wired to track the child's turn index, this value should
            // match the frontier at failure time.
            match journal.save_checkpoint(agent_id, 0, checkpoint_data) {
                Ok(()) => {
                    tracing::info!(
                        "gen_ai.agent.name" = agent_id.0.as_str(),
                        failure_reason = failure_reason,
                        "SnapshotAndFail: checkpoint persisted via journal.save_checkpoint"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "gen_ai.agent.name" = agent_id.0.as_str(),
                        failure_reason = failure_reason,
                        error = %e,
                        "SnapshotAndFail: save_checkpoint failed — falling back to \
                         LetCrash semantics (no durable snapshot)"
                    );
                }
            }
        } else {
            tracing::warn!(
                "gen_ai.agent.name" = agent_id.0.as_str(),
                failure_reason = failure_reason,
                "SnapshotAndFail: checkpoint data built but NOT persisted — \
                 supervisor has no journal_storage wired. Behaviour equivalent to \
                 LetCrash. TODO: wire set_journal_storage at construction."
            );
        }
    }
}

/// Spawn a retry task that handles its own failures recursively.
///
/// This is extracted as a free function so it can recurse from inside
/// a tokio::spawn (where we can't add to the parent JoinSet).
pub(super) fn spawn_retry_task(
    factory: Arc<dyn TaskFactory>,
    config: SpawnConfig,
    parent_budget: Arc<Mutex<ResourceBudget>>,
    retry_counts: Arc<Mutex<HashMap<AgentId, usize>>>,
) {
    let agent_id = config.agent_id.clone();
    let restart_strategy = config.restart_strategy.clone();
    let new_token = CancellationToken::new(Duration::from_secs(5));
    let retry_config = config.clone();
    let new_future = factory.create_task(config, new_token);

    tokio::spawn(async move {
        match new_future.await {
            Ok(output) => {
                let mut budget = parent_budget.lock().unwrap();
                budget.used_tokens +=
                    output.token_usage.input_tokens + output.token_usage.output_tokens;
                budget.used_turns += output.used_turns;
                budget.used_cost += output.used_cost;
            }
            Err(_err) => {
                // Check if we should retry again
                let should_retry = match restart_strategy {
                    RestartStrategy::RetryOnce => {
                        let mut counts = retry_counts.lock().unwrap();
                        let count = counts.entry(agent_id.clone()).or_insert(0);
                        if *count < 1 {
                            *count += 1;
                            true
                        } else {
                            false
                        }
                    }
                    RestartStrategy::RetryTwiceThenFail => {
                        let mut counts = retry_counts.lock().unwrap();
                        let count = counts.entry(agent_id.clone()).or_insert(0);
                        if *count < 2 {
                            *count += 1;
                            true
                        } else {
                            false
                        }
                    }
                    _ => false,
                };
                if should_retry {
                    spawn_retry_task(factory, retry_config, parent_budget, retry_counts);
                }
            }
        }
    });
}
