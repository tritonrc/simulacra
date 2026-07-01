use super::restart::spawn_retry_task;
use super::*;

impl AgentSupervisor {
    /// Run the supervisor actor loop.
    ///
    /// Receives `SupervisorMessage` values from the `mpsc::Receiver` and
    /// dispatches them using `tokio::select!`. Child agents are launched
    /// via `tokio::spawn` and tracked in the children map.
    ///
    /// The loop exits when the channel closes (all senders dropped) OR when
    /// all spawned child tasks have completed and there are no more pending
    /// messages to process.
    pub async fn run_actor_loop(&self, mut rx: tokio::sync::mpsc::Receiver<SupervisorMessage>) {
        use tokio::sync::mpsc::error::TryRecvError;

        let mut priority_queue: BinaryHeap<SupervisorMessage> = BinaryHeap::new();
        let mut task_set = tokio::task::JoinSet::<()>::new();
        let mut ever_dispatched = false;

        loop {
            // If we have active tasks, select on both new messages and task completion
            if !task_set.is_empty() {
                tokio::select! {
                    biased;
                    msg = rx.recv() => {
                        match msg {
                            Some(supervisor_msg) => {
                                priority_queue.push(supervisor_msg);
                                while let Ok(extra) = rx.try_recv() {
                                    priority_queue.push(extra);
                                }
                                while let Some(queued) = priority_queue.pop() {
                                    self.dispatch_message_into(&mut task_set, queued).await;
                                    ever_dispatched = true;
                                }
                            }
                            None => {
                                // Channel closed — drain remaining tasks before exiting
                                // so that spawned children can send their results back.
                                while task_set.join_next().await.is_some() {}
                                break;
                            }
                        }
                    }
                    _ = task_set.join_next() => {
                        // A task completed. If all tasks are done and no pending
                        // messages, check if we should exit.
                        if task_set.is_empty() {
                            match rx.try_recv() {
                                Ok(msg) => {
                                    priority_queue.push(msg);
                                    while let Ok(extra) = rx.try_recv() {
                                        priority_queue.push(extra);
                                    }
                                    while let Some(queued) = priority_queue.pop() {
                                        self.dispatch_message_into(&mut task_set, queued).await;
                                        ever_dispatched = true;
                                    }
                                }
                                Err(TryRecvError::Empty) if ever_dispatched => break,
                                Err(TryRecvError::Disconnected) => break,
                                Err(TryRecvError::Empty) => {
                                    // Haven't dispatched yet, keep waiting
                                }
                            }
                        }
                    }
                }
            } else if ever_dispatched {
                // All tasks finished and we've dispatched before — check for more
                match rx.try_recv() {
                    Ok(msg) => {
                        priority_queue.push(msg);
                        while let Ok(extra) = rx.try_recv() {
                            priority_queue.push(extra);
                        }
                        while let Some(queued) = priority_queue.pop() {
                            self.dispatch_message_into(&mut task_set, queued).await;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            } else {
                // No tasks yet, wait for first message
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Some(supervisor_msg) => {
                                priority_queue.push(supervisor_msg);
                                while let Ok(extra) = rx.try_recv() {
                                    priority_queue.push(extra);
                                }
                                while let Some(queued) = priority_queue.pop() {
                                    self.dispatch_message_into(&mut task_set, queued).await;
                                    ever_dispatched = true;
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        }
    }

    /// Spawn a task via the factory into the JoinSet and return its handle info.
    fn spawn_task_into(
        &self,
        task_set: &mut tokio::task::JoinSet<()>,
        config: SpawnConfig,
        result_tx: tokio::sync::oneshot::Sender<SpawnResult>,
    ) {
        if let Some(ref factory) = self.task_factory {
            let agent_id = config.agent_id.clone();
            let restart_strategy = config.restart_strategy.clone();
            let retry_config = config.clone();
            let token = CancellationToken::new(Duration::from_secs(5));
            let task_future = factory.create_task(config, token.clone());
            let parent_budget = Arc::clone(&self.parent_budget);
            let factory_clone = Arc::clone(factory);
            let retry_counts = Arc::clone(&self.retry_counts_shared);

            self.cancellation_tokens
                .lock()
                .unwrap()
                .insert(agent_id.clone(), token);

            task_set.spawn(async move {
                match task_future.await {
                    Ok(output) => {
                        // Roll up child budget from actual AgentLoopOutput,
                        // not the stale SpawnConfig clone (S018 fix).
                        {
                            let mut budget = parent_budget.lock().unwrap();
                            budget.used_tokens +=
                                output.token_usage.input_tokens + output.token_usage.output_tokens;
                            budget.used_turns += output.used_turns;
                            budget.used_cost += output.used_cost;
                        }
                        // Send result back to the spawn_agent caller.
                        let _ = result_tx.send(Ok(output));
                    }
                    Err(err) => {
                        // Check restart strategy
                        let should_restart = match restart_strategy {
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
                        if should_restart {
                            spawn_retry_task(
                                factory_clone,
                                retry_config,
                                parent_budget,
                                retry_counts,
                            );
                        }
                        // Send error back to the spawn_agent caller.
                        let _ = result_tx.send(Err(err));
                    }
                }
            });
        } else {
            // No factory — send error back immediately.
            let _ = result_tx.send(Err(RuntimeError::Session(
                "supervisor has no task factory configured".into(),
            )));
        }
    }
}

impl AgentSupervisor {
    /// Dispatch a single supervisor message, spawning tasks into the given JoinSet.
    ///
    /// For Spawn payloads, runs the same validation, journaling, tracing, and
    /// activity events as the direct `spawn_agent()` path via
    /// `validate_and_prepare_spawn()`.
    async fn dispatch_message_into(
        &self,
        task_set: &mut tokio::task::JoinSet<()>,
        msg: SupervisorMessage,
    ) {
        match msg.payload {
            SupervisorPayload::Spawn(config, result_tx) => {
                if let Err(err) = self.validate_and_prepare_spawn(&config) {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                self.spawn_task_into(task_set, *config, result_tx);
            }
            SupervisorPayload::Cancel => {
                if let Some(token) = self.cancellation_tokens.lock().unwrap().get(&msg.agent_id) {
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
    async fn cancel_with_grace(&self, agent_id: &AgentId, token: &CancellationToken) {
        token.signal();
        let grace_duration = token.grace();

        let handle = self.children.lock().unwrap().remove(agent_id);
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
    fn abort_child(&self, agent_id: &AgentId) {
        if let Some(handle) = self.children.lock().unwrap().remove(agent_id) {
            handle.abort();
        }
    }
}
