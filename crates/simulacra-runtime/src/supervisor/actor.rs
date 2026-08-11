use super::restart::run_task_with_retries;
use super::spawn::{await_awaiting_approval_terminal, count_tool_uses, status_from_spawn_result};
use super::*;
use tracing::Instrument;

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
                        // A task completed. If all tasks are done, drain any
                        // queued messages and continue so late joins/cancels
                        // remain available until all senders are dropped.
                        if task_set.is_empty() {
                            match rx.try_recv() {
                                Ok(msg) => priority_queue.push(msg),
                                Err(TryRecvError::Empty) => {}
                                Err(TryRecvError::Disconnected) => break,
                            }
                            while let Ok(extra) = rx.try_recv() {
                                priority_queue.push(extra);
                            }
                            while let Some(queued) = priority_queue.pop() {
                                self.dispatch_message_into(&mut task_set, queued).await;
                                ever_dispatched = true;
                            }
                            if task_set.is_empty() {
                                continue;
                            }
                        }
                    }
                }
            } else if ever_dispatched {
                // All tasks finished, but the supervisor must stay alive for
                // late joins/cancels and future child spawns until senders drop.
                match rx.recv().await {
                    Some(msg) => {
                        priority_queue.push(msg);
                        while let Ok(extra) = rx.try_recv() {
                            priority_queue.push(extra);
                        }
                        while let Some(queued) = priority_queue.pop() {
                            self.dispatch_message_into(&mut task_set, queued).await;
                        }
                    }
                    None => break,
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
    pub(super) fn spawn_task_into(
        &self,
        task_set: &mut tokio::task::JoinSet<()>,
        config: SpawnConfig,
        factory: Arc<dyn TaskFactory>,
        create_agent_span: Option<tracing::Span>,
        accepted_budget: super::spawn::AcceptedBudgetReservation,
    ) {
        let agent_id = config.agent_id.clone();
        let token = CancellationToken::new(Duration::from_secs(5));
        let (input_queue, input_handle) = AgentInputQueue::new();
        let task_future = factory.create_task_with_input_and_budget(
            config.clone(),
            token.clone(),
            input_queue,
            accepted_budget.child_budget,
        );
        let started_at_ms = now_ms();
        let result_context =
            self.child_result_context(&config, Instant::now(), accepted_budget.parent_budget);
        let retry_counts = Arc::clone(&self.retry_counts_shared);
        let child_results = Arc::clone(&self.child_results);
        let cancellation_tokens = Arc::clone(&self.cancellation_tokens);
        let child_inputs = Arc::clone(&self.child_inputs);
        let budget_reservations = Arc::clone(&self.budget_reservations);
        let lifecycle_cancellation = token.clone();

        lock_mutex(&self.cancellation_tokens, "cancellation_tokens")
            .insert(agent_id.clone(), token);
        lock_mutex(&self.child_inputs, "child_inputs").insert(agent_id.clone(), input_handle);
        lock_mutex(&self.child_results, "child_results").insert(
            agent_id.clone(),
            ChildRunState {
                metadata: child_metadata(&config, started_at_ms),
                result: None,
                result_delivered: false,
                join_waiters: Vec::new(),
                wait_waiters: Vec::new(),
            },
        );

        let child_lifecycle = async move {
            let result = run_task_with_retries(
                Arc::clone(&factory),
                config.clone(),
                task_future,
                retry_counts,
            )
            .await;
            let result = await_awaiting_approval_terminal(result, lifecycle_cancellation).await;
            let _after_hook_outcome = factory.after_spawn(&config, &result);
            AgentSupervisor::settle_budget_reservation(
                &budget_reservations,
                &result_context.agent_id,
                &result,
            );
            let result =
                AgentSupervisor::process_child_result_after_settlement(result, &result_context);
            let terminal = ChildTerminalResult {
                child_id: result_context.agent_id.clone(),
                placement: result_context.placement.clone(),
                status: status_from_spawn_result(&result),
                elapsed_ms: result_context.spawn_start.elapsed().as_millis() as u64,
                tool_uses: result.as_ref().map(count_tool_uses).unwrap_or(0),
                result: result.map_err(|err| err.to_string()),
            };
            AgentSupervisor::record_child_terminal_result(&child_results, terminal);
            lock_mutex(&cancellation_tokens, "cancellation_tokens")
                .remove(&result_context.agent_id);
            lock_mutex(&child_inputs, "child_inputs").remove(&result_context.agent_id);
        };
        if let Some(span) = create_agent_span {
            task_set.spawn(child_lifecycle.instrument(span));
        } else {
            task_set.spawn(child_lifecycle);
        }
    }
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) fn child_metadata(config: &SpawnConfig, started_at_ms: u64) -> ChildMetadata {
    ChildMetadata {
        child_id: config.agent_id.clone(),
        placement: config.placement.clone(),
        task: config.task.clone(),
        parent_id: config.parent_id.clone(),
        capability: config.capability.clone().unwrap_or_default(),
        started_at_ms,
        finished_at_ms: None,
    }
}
