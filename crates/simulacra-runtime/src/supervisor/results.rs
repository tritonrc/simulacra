use super::*;

impl AgentSupervisor {
    /// Cancel a running agent by signalling its cancellation token.
    ///
    /// The agent receives a cooperative cancellation signal and has a grace
    /// period to finish its current work before forceful termination.
    pub fn cancel_agent(&self, token: &CancellationToken) {
        token.signal();
        let _grace = token.grace();
        tracing::info!("agent cancellation signalled");
    }

    /// Queue steering input for a live child agent.
    pub fn steer_child(&self, child_id: &AgentId, message: String) -> Result<(), String> {
        if let Some(handle) = lock_mutex(&self.child_inputs, "child_inputs").get(child_id) {
            handle.enqueue(message)
        } else if lock_mutex(&self.child_results, "child_results")
            .get(child_id)
            .is_some_and(|state| state.result.is_some())
        {
            Err(format!("child_id already completed: {}", child_id.0))
        } else {
            Err(format!("unknown child_id: {}", child_id.0))
        }
    }

    /// Deduct a child agent's resource usage from the parent budget.
    ///
    /// S006: When a child agent completes, its consumed tokens, turns, and cost
    /// must be rolled up into the parent so the parent's remaining budget
    /// accurately reflects total work performed.
    pub fn deduct_child_usage(&self, config: &SpawnConfig) {
        let mut budget = lock_mutex(&self.parent_budget, "parent_budget");
        budget.used_tokens += config.budget.used_tokens;
        budget.used_turns += config.budget.used_turns;
        budget.used_cost += config.budget.used_cost;
    }

    /// Returns a snapshot of the parent budget.
    pub fn parent_budget(&self) -> ResourceBudget {
        lock_mutex(&self.parent_budget, "parent_budget").clone()
    }

    /// Handle a child agent's successful completion.
    ///
    /// Deducts the child's resource usage (tokens, turns, cost) from the parent
    /// budget so the parent's remaining budget accurately reflects total work.
    pub fn handle_completion(&self, config: &SpawnConfig) {
        self.deduct_child_usage(config);
        tracing::info!(
            "gen_ai.agent.name" = config.agent_id.0.as_str(),
            "child agent completed, budget deducted from parent"
        );
    }
}
