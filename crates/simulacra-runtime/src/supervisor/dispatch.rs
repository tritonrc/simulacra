use super::*;

impl AgentSupervisor {
    /// Dispatch a single supervisor message, spawning tasks into the given JoinSet.
    ///
    /// For Spawn payloads, runs the same validation, journaling, tracing, and
    /// activity events as the direct `spawn_agent()` path via
    /// `validate_and_prepare_spawn()`.
    pub(super) async fn dispatch_message_into(
        &self,
        task_set: &mut tokio::task::JoinSet<()>,
        msg: SupervisorMessage,
    ) {
        match msg.payload {
            SupervisorPayload::Spawn(config, result_tx) => {
                if let Err(err) = self.validate_spawn_request(&config) {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                let Some(factory) = self.task_factory.clone() else {
                    let _ = result_tx.send(Err(RuntimeError::SpawnMissingTask));
                    return;
                };
                if let Err(err) = factory.validate_spawn_config(&config) {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                if let Err(err) = self.prepare_spawn(&config) {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                let ack = SpawnAck {
                    child_id: config.agent_id.clone(),
                    agent_type: config
                        .agent_type
                        .clone()
                        .unwrap_or_else(|| "generic".to_string()),
                };
                let _ = result_tx.send(Ok(ack));
                self.spawn_task_into(task_set, *config, factory);
            }
            SupervisorPayload::JoinChild(child_id, result_tx) => {
                self.join_child(child_id, result_tx);
            }
            SupervisorPayload::ChildStatus(child_id, result_tx) => {
                let result = self.child_status(&child_id);
                let _ = result_tx.send(result);
            }
            SupervisorPayload::ListChildren(result_tx) => {
                let _ = result_tx.send(Ok(self.list_children()));
            }
            SupervisorPayload::WaitChild(child_id, timeout, result_tx) => {
                self.wait_child(child_id, timeout, result_tx);
            }
            SupervisorPayload::WaitChildren(child_ids, timeout, result_tx) => {
                self.wait_children(child_ids, timeout, result_tx);
            }
            SupervisorPayload::CloseChild(child_id, result_tx) => {
                let result = self.close_child(&child_id);
                let _ = result_tx.send(result);
            }
            SupervisorPayload::CancelChild(child_id, result_tx) => {
                let result = if let Some(token) =
                    lock_mutex(&self.cancellation_tokens, "cancellation_tokens").get(&child_id)
                {
                    token.signal();
                    Ok(())
                } else if lock_mutex(&self.child_results, "child_results")
                    .get(&child_id)
                    .is_some_and(|state| state.result.is_some())
                {
                    Err(format!("child_id already completed: {}", child_id.0))
                } else {
                    Err(format!("unknown child_id: {}", child_id.0))
                };
                let _ = result_tx.send(result);
            }
            SupervisorPayload::SteerChild(child_id, message, result_tx) => {
                let result = if let Some(handle) =
                    lock_mutex(&self.child_inputs, "child_inputs").get(&child_id)
                {
                    handle.enqueue(message)
                } else if lock_mutex(&self.child_results, "child_results")
                    .get(&child_id)
                    .is_some_and(|state| state.result.is_some())
                {
                    Err(format!("child_id already completed: {}", child_id.0))
                } else {
                    Err(format!("unknown child_id: {}", child_id.0))
                };
                let _ = result_tx.send(result);
            }
            SupervisorPayload::Cancel => {
                if let Some(token) =
                    lock_mutex(&self.cancellation_tokens, "cancellation_tokens").get(&msg.agent_id)
                {
                    token.signal();
                }
            }
            SupervisorPayload::Completed => {
                // Budget rollup handled in the spawned task
            }
            SupervisorPayload::Failed(_reason) => {
                // Failure restart handled in the spawned task
            }
        }
    }

    /// Cancel a running agent with a grace period before forceful abort.
    ///
    /// Uses `tokio::time::timeout` with the token's `grace()` duration.
    /// If the agent does not shut down within the grace period, the task
    /// handle is forcefully terminated via `abort`.
    #[allow(dead_code)]
    pub(super) async fn cancel_with_grace(&self, agent_id: &AgentId, token: &CancellationToken) {
        token.signal();
        let grace_duration = token.grace();

        let handle = lock_mutex(&self.children, "children").remove(agent_id);
        if let Some(handle) = handle {
            // Grab an AbortHandle before passing the JoinHandle to the timeout
            // future. If the timeout expires, the JoinHandle is dropped (which
            // detaches the task), so we need the AbortHandle to still be able
            // to cancel it.
            let abort_handle = handle.abort_handle();
            let result = tokio::time::timeout(grace_duration, handle).await;
            if result.is_err() {
                // Grace period expired — forcefully terminate via the
                // AbortHandle we retained. We intentionally do NOT call
                // abort_child here because the handle was already removed
                // from self.children above and abort_child would find
                // nothing, letting the task detach.
                tracing::warn!("agent did not shut down within grace period, aborting");
                abort_handle.abort();
            }
        }
    }

    /// Abort a child task forcefully (used after grace period expiry).
    #[allow(dead_code)]
    pub(super) fn abort_child(&self, agent_id: &AgentId) {
        if let Some(handle) = lock_mutex(&self.children, "children").remove(agent_id) {
            handle.abort();
        }
    }
}
