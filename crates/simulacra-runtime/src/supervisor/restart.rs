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
        match strategy {
            RestartStrategy::RetryOnce | RestartStrategy::RetryTwiceThenFail => {
                should_retry(agent_id, strategy, failure_reason, &self.retry_counts)
            }
            RestartStrategy::SnapshotAndFail => {
                self.snapshot_before_fail(agent_id, failure_reason);
                false
            }
            RestartStrategy::LetCrash => false,
        }
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
            budget_snapshot: lock_mutex(&self.parent_budget, "parent_budget").clone(),
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

pub(super) async fn run_task_with_retries(
    factory: Arc<dyn TaskFactory>,
    config: SpawnConfig,
    first_future: BoxTaskFuture,
    retry_counts: Arc<Mutex<HashMap<AgentId, usize>>>,
) -> SpawnResult {
    let agent_id = config.agent_id.clone();
    let restart_strategy = config.restart_strategy.clone();
    let mut result = first_future.await;

    while let Err(err) = &result {
        if !should_retry(
            &agent_id,
            &restart_strategy,
            &err.to_string(),
            &retry_counts,
        ) {
            break;
        }
        let token = CancellationToken::new(Duration::from_secs(5));
        result = factory.create_task(config.clone(), token).await;
    }

    result
}

fn should_retry(
    agent_id: &AgentId,
    strategy: &RestartStrategy,
    failure_reason: &str,
    retry_counts: &Mutex<HashMap<AgentId, usize>>,
) -> bool {
    let limit = match strategy {
        RestartStrategy::RetryOnce => 1,
        RestartStrategy::RetryTwiceThenFail => 2,
        RestartStrategy::SnapshotAndFail | RestartStrategy::LetCrash => return false,
    };

    let mut counts = lock_mutex(retry_counts, "retry_counts");
    let count = counts.entry(agent_id.clone()).or_insert(0);
    if *count < limit {
        *count += 1;
        tracing::warn!(
            "gen_ai.agent.name" = agent_id.0.as_str(),
            strategy = strategy_name(strategy),
            failure_reason = failure_reason,
            retry_count = *count,
            "agent restart triggered"
        );
        true
    } else {
        false
    }
}

fn strategy_name(strategy: &RestartStrategy) -> &'static str {
    match strategy {
        RestartStrategy::RetryOnce => "retry_once",
        RestartStrategy::RetryTwiceThenFail => "retry_twice_then_fail",
        RestartStrategy::SnapshotAndFail => "snapshot_and_fail",
        RestartStrategy::LetCrash => "let_crash",
    }
}
