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
            SupervisorPayload::Spawn(mut config, result_tx) => {
                if let Err(err) = self.require_bound_root() {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                if msg.agent_id != config.parent_id {
                    let _ = result_tx.send(Err(RuntimeError::CapabilityViolation(format!(
                        "caller {:?} cannot spawn as parent {:?}",
                        msg.agent_id.0, config.parent_id.0
                    ))));
                    return;
                }
                if let Err(err) = self.ensure_child_id_is_new(&config.agent_id) {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                if let Err(err) = self.validate_spawn_authorization(&config) {
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
                if let Err(err) = self.require_spawn_journal() {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                if let Err(err) = self.validate_spawn_budget(&config) {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                let caller_capability = match self.capability_for_caller(&config.parent_id) {
                    Ok(capability) => capability,
                    Err(err) => {
                        let _ = result_tx.send(Err(err));
                        return;
                    }
                };
                if let Err(err) =
                    factory.prepare_spawn_config_for_caller(&mut config, &caller_capability)
                {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                if let Err(err) = self.validate_spawn_authorization(&config) {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                if let Err(err) = self.validate_spawn_budget(&config) {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                let backend = factory.placement_backend(&config);
                let parent_span = take_spawn_parent_span(&config.parent_id, &config.agent_id);
                let create_agent_span =
                    Self::create_agent_span(&config, backend, parent_span.as_ref());
                let accepted_budget = match self.reserve_spawn_budget(&config) {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        let _ = result_tx.send(Err(error));
                        return;
                    }
                };
                let prepare_result = {
                    let _guard = create_agent_span.enter();
                    self.prepare_spawn(&config, backend)
                };
                if let Err(err) = prepare_result {
                    self.rollback_unaccepted_budget(&config.agent_id);
                    let _ = result_tx.send(Err(err));
                    return;
                }
                self.record_accepted_child_id(&config.agent_id);
                let ack = SpawnAck {
                    child_id: config.agent_id.clone(),
                    placement: config.placement.clone(),
                    backend,
                };
                let _ = result_tx.send(Ok(ack));
                self.spawn_task_into(
                    task_set,
                    *config,
                    factory,
                    Some(create_agent_span),
                    accepted_budget,
                );
            }
            SupervisorPayload::JoinChild(child_id, result_tx) => {
                if let Err(error) = self.authorize_child_owner(&msg.agent_id, &child_id) {
                    let _ = result_tx.send(Err(error));
                } else {
                    self.join_child(child_id, result_tx);
                }
            }
            SupervisorPayload::InspectChildResult(child_id, result_tx) => {
                let result = self
                    .validate_caller_identity(&msg.agent_id)
                    .and_then(|()| self.inspect_child_result(&child_id));
                let _ = result_tx.send(result);
            }
            SupervisorPayload::ChildStatus(child_id, result_tx) => {
                if let Err(error) = self.authorize_child_owner(&msg.agent_id, &child_id) {
                    let _ = result_tx.send(Err(error));
                } else {
                    self.send_child_status(&child_id, result_tx);
                }
            }
            SupervisorPayload::ListChildren(result_tx) => {
                self.send_child_roster(&msg.agent_id, result_tx);
            }
            SupervisorPayload::InspectChildren(result_tx) => {
                self.send_child_roster_inspection(result_tx);
            }
            SupervisorPayload::WaitChild(child_id, timeout, result_tx) => {
                if let Err(error) = self.authorize_child_owner(&msg.agent_id, &child_id) {
                    let _ = result_tx.send(Err(error));
                } else {
                    self.wait_child(child_id, timeout, result_tx);
                }
            }
            SupervisorPayload::WaitChildren(child_ids, timeout, result_tx) => {
                let authorization = child_ids
                    .iter()
                    .try_for_each(|child_id| self.authorize_child_owner(&msg.agent_id, child_id));
                if let Err(error) = authorization {
                    let _ = result_tx.send(Err(error));
                } else {
                    self.wait_children(child_ids, timeout, result_tx);
                }
            }
            SupervisorPayload::CloseChild(child_id, result_tx) => {
                let result = self
                    .authorize_child_owner(&msg.agent_id, &child_id)
                    .and_then(|()| self.close_child(&child_id));
                let _ = result_tx.send(result);
            }
            SupervisorPayload::CancelChild(child_id, result_tx) => {
                let result =
                    if let Err(error) = self.authorize_child_owner(&msg.agent_id, &child_id) {
                        Err(error)
                    } else if let Some(token) =
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
                let result =
                    if let Err(error) = self.authorize_child_owner(&msg.agent_id, &child_id) {
                        Err(error)
                    } else if let Some(handle) =
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
