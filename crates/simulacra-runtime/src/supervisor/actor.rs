use super::restart::run_task_with_retries;
use super::spawn::{count_tool_uses, status_from_spawn_result};
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
    fn spawn_task_into(
        &self,
        task_set: &mut tokio::task::JoinSet<()>,
        config: SpawnConfig,
        factory: Arc<dyn TaskFactory>,
    ) {
        let agent_id = config.agent_id.clone();
        let token = CancellationToken::new(Duration::from_secs(5));
        let (input_queue, input_handle) = AgentInputQueue::new();
        let task_future =
            factory.create_task_with_input(config.clone(), token.clone(), input_queue);
        let started_at_ms = now_ms();
        let result_context = self.child_result_context(&config, Instant::now());
        let retry_counts = Arc::clone(&self.retry_counts_shared);
        let child_results = Arc::clone(&self.child_results);
        let cancellation_tokens = Arc::clone(&self.cancellation_tokens);
        let child_inputs = Arc::clone(&self.child_inputs);

        lock_mutex(&self.cancellation_tokens, "cancellation_tokens")
            .insert(agent_id.clone(), token);
        lock_mutex(&self.child_inputs, "child_inputs").insert(agent_id.clone(), input_handle);
        lock_mutex(&self.child_results, "child_results").insert(
            agent_id.clone(),
            ChildRunState {
                metadata: child_metadata(&config, started_at_ms),
                result: None,
                join_waiters: Vec::new(),
                wait_waiters: Vec::new(),
            },
        );

        task_set.spawn(async move {
            let result = run_task_with_retries(factory, config, task_future, retry_counts).await;
            let result = AgentSupervisor::process_child_result(result, &result_context);
            let terminal = ChildTerminalResult {
                child_id: result_context.agent_id.clone(),
                agent_type: result_context.agent_type.clone(),
                status: status_from_spawn_result(&result),
                elapsed_ms: result_context.spawn_start.elapsed().as_millis() as u64,
                tool_uses: result.as_ref().map(count_tool_uses).unwrap_or(0),
                result: result.map_err(|err| err.to_string()),
            };
            AgentSupervisor::record_child_terminal_result(&child_results, terminal);
            lock_mutex(&cancellation_tokens, "cancellation_tokens")
                .remove(&result_context.agent_id);
            lock_mutex(&child_inputs, "child_inputs").remove(&result_context.agent_id);
        });
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
                if let Err(err) = self.validate_spawn_request(&config) {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                let Some(factory) = self.task_factory.clone() else {
                    let _ = result_tx.send(Err(RuntimeError::SpawnMissingTask));
                    return;
                };
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
    async fn cancel_with_grace(&self, agent_id: &AgentId, token: &CancellationToken) {
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
    fn abort_child(&self, agent_id: &AgentId) {
        if let Some(handle) = lock_mutex(&self.children, "children").remove(agent_id) {
            handle.abort();
        }
    }

    fn join_child(&self, child_id: AgentId, result_tx: ChildJoinSender) {
        let mut results = lock_mutex(&self.child_results, "child_results");
        let Some(state) = results.get_mut(&child_id) else {
            let _ = result_tx.send(Err(format!("unknown child_id: {}", child_id.0)));
            return;
        };
        if let Some(result) = state.result.clone() {
            let _ = result_tx.send(Ok(result));
        } else {
            state.join_waiters.push(result_tx);
        }
    }

    fn child_status(&self, child_id: &AgentId) -> Result<ChildStatus, String> {
        let results = lock_mutex(&self.child_results, "child_results");
        let Some(state) = results.get(child_id) else {
            return Err(format!("unknown or closed child_id: {}", child_id.0));
        };
        Ok(status_from_state(state))
    }

    fn wait_child(&self, child_id: AgentId, timeout: Duration, result_tx: ChildWaitSender) {
        let mut results = lock_mutex(&self.child_results, "child_results");
        let Some(state) = results.get_mut(&child_id) else {
            let _ = result_tx.send(Err(format!("unknown or closed child_id: {}", child_id.0)));
            return;
        };
        if let Some(terminal) = state.result.clone() {
            let _ = result_tx.send(Ok(wait_result_terminal(terminal)));
            return;
        }
        if timeout.is_zero() {
            let _ = result_tx.send(Ok(wait_result_running(&state.metadata)));
            return;
        }

        let waiter_id = self.wait_counter.fetch_add(1, Ordering::Relaxed);
        state.wait_waiters.push(ChildWaiter {
            id: waiter_id,
            child_ids: vec![child_id.clone()],
            sender: Arc::new(Mutex::new(Some(ChildWaiterSender::Single(result_tx)))),
        });
        drop(results);

        let child_results = Arc::clone(&self.child_results);
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let sender_and_metadata = {
                let mut results = lock_mutex(&child_results, "child_results");
                let Some(state) = results.get_mut(&child_id) else {
                    return;
                };
                let Some(position) = state
                    .wait_waiters
                    .iter()
                    .position(|waiter| waiter.id == waiter_id)
                else {
                    return;
                };
                let waiter = state.wait_waiters.swap_remove(position);
                let sender = waiter
                    .sender
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                (sender, state.metadata.clone())
            };
            let (sender, metadata) = sender_and_metadata;
            if let Some(ChildWaiterSender::Single(sender)) = sender {
                let _ = sender.send(Ok(wait_result_running(&metadata)));
            }
        });
    }

    // Timeout and terminal completion intentionally race to take the same
    // sender. A timeout may return "running" even if completion is being
    // recorded concurrently; callers can re-wait or join without losing data.
    fn wait_children(
        &self,
        child_ids: Vec<AgentId>,
        timeout: Duration,
        result_tx: ChildrenWaitSender,
    ) {
        if child_ids.is_empty() {
            let _ = result_tx.send(Err("missing required field: child_ids".into()));
            return;
        }

        let mut results = lock_mutex(&self.child_results, "child_results");
        for child_id in &child_ids {
            if !results.contains_key(child_id) {
                let _ = result_tx.send(Err(format!("unknown or closed child_id: {}", child_id.0)));
                return;
            }
        }

        for child_id in &child_ids {
            let state = results
                .get(child_id)
                .expect("child existence checked before terminal scan");
            if let Some(terminal) = state.result.clone() {
                let _ = result_tx.send(Ok(wait_children_result_terminal(child_ids, terminal)));
                return;
            }
        }

        if timeout.is_zero() {
            let _ = result_tx.send(Ok(wait_children_result_running(child_ids)));
            return;
        }

        let waiter_id = self.wait_counter.fetch_add(1, Ordering::Relaxed);
        let shared_sender = Arc::new(Mutex::new(Some(ChildWaiterSender::Any(result_tx))));
        for child_id in &child_ids {
            let state = results
                .get_mut(child_id)
                .expect("child existence checked before waiter registration");
            state.wait_waiters.push(ChildWaiter {
                id: waiter_id,
                child_ids: child_ids.clone(),
                sender: Arc::clone(&shared_sender),
            });
        }
        drop(results);

        let child_results = Arc::clone(&self.child_results);
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let sender = {
                let mut results = lock_mutex(&child_results, "child_results");
                for child_id in &child_ids {
                    let Some(state) = results.get_mut(child_id) else {
                        continue;
                    };
                    state.wait_waiters.retain(|waiter| waiter.id != waiter_id);
                }
                shared_sender
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
            };
            if let Some(ChildWaiterSender::Any(sender)) = sender {
                let _ = sender.send(Ok(wait_children_result_running(child_ids)));
            }
        });
    }

    fn close_child(&self, child_id: &AgentId) -> Result<(), String> {
        let mut results = lock_mutex(&self.child_results, "child_results");
        let Some(state) = results.get(child_id) else {
            return Err(format!("unknown or closed child_id: {}", child_id.0));
        };
        if state.result.is_none() {
            return Err(format!("child_id is still running: {}", child_id.0));
        }
        results.remove(child_id);
        drop(results);
        lock_mutex(&self.cancellation_tokens, "cancellation_tokens").remove(child_id);
        lock_mutex(&self.child_inputs, "child_inputs").remove(child_id);
        Ok(())
    }

    pub(super) fn record_child_terminal_result(
        child_results: &Arc<Mutex<HashMap<AgentId, ChildRunState>>>,
        mut terminal: ChildTerminalResult,
    ) {
        let (join_waiters, wait_waiters) = {
            let mut results = lock_mutex(child_results, "child_results");
            let finished_at_ms = now_ms();
            let fallback_metadata = ChildMetadata {
                child_id: terminal.child_id.clone(),
                agent_type: terminal.agent_type.clone(),
                task: String::new(),
                parent_id: AgentId(String::new()),
                started_at_ms: finished_at_ms,
                finished_at_ms: None,
            };
            let had_state = results.contains_key(&terminal.child_id);
            let state = results
                .entry(terminal.child_id.clone())
                .or_insert_with(|| ChildRunState {
                    metadata: fallback_metadata,
                    result: None,
                    join_waiters: Vec::new(),
                    wait_waiters: Vec::new(),
                });
            state.metadata.finished_at_ms = Some(finished_at_ms);
            if had_state {
                terminal.elapsed_ms = finished_at_ms.saturating_sub(state.metadata.started_at_ms);
            }
            state.result = Some(terminal.clone());
            let join_waiters = std::mem::take(&mut state.join_waiters);
            let wait_waiters = std::mem::take(&mut state.wait_waiters);
            for waiter in &wait_waiters {
                for waited_child_id in &waiter.child_ids {
                    if waited_child_id == &terminal.child_id {
                        continue;
                    }
                    if let Some(waited_state) = results.get_mut(waited_child_id) {
                        waited_state
                            .wait_waiters
                            .retain(|candidate| candidate.id != waiter.id);
                    }
                }
            }
            (join_waiters, wait_waiters)
        };
        for waiter in join_waiters {
            let _ = waiter.send(Ok(terminal.clone()));
        }
        for waiter in wait_waiters {
            let sender = waiter
                .sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            match sender {
                Some(ChildWaiterSender::Single(sender)) => {
                    let _ = sender.send(Ok(wait_result_terminal(terminal.clone())));
                }
                Some(ChildWaiterSender::Any(sender)) => {
                    let _ = sender.send(Ok(wait_children_result_terminal(
                        waiter.child_ids,
                        terminal.clone(),
                    )));
                }
                None => {}
            }
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
        agent_type: config
            .agent_type
            .clone()
            .unwrap_or_else(|| "generic".to_string()),
        task: config.task.clone(),
        parent_id: config.parent_id.clone(),
        started_at_ms,
        finished_at_ms: None,
    }
}

fn status_from_terminal(terminal: &ChildTerminalResult) -> String {
    terminal.status.clone()
}

fn status_from_state(state: &ChildRunState) -> ChildStatus {
    let ready = state.result.is_some();
    let status = state
        .result
        .as_ref()
        .map(status_from_terminal)
        .unwrap_or_else(|| "running".to_string());
    let end_ms = state.metadata.finished_at_ms.unwrap_or_else(now_ms);
    ChildStatus {
        child_id: state.metadata.child_id.clone(),
        agent_type: state.metadata.agent_type.clone(),
        status,
        ready,
        elapsed_ms: end_ms.saturating_sub(state.metadata.started_at_ms),
    }
}

fn wait_result_running(metadata: &ChildMetadata) -> WaitChildResult {
    WaitChildResult {
        child_id: metadata.child_id.clone(),
        agent_type: None,
        status: "running".to_string(),
        ready: false,
        terminal: None,
    }
}

fn wait_result_terminal(terminal: ChildTerminalResult) -> WaitChildResult {
    WaitChildResult {
        child_id: terminal.child_id.clone(),
        agent_type: Some(terminal.agent_type.clone()),
        status: status_from_terminal(&terminal),
        ready: true,
        terminal: Some(terminal),
    }
}

fn wait_children_result_running(child_ids: Vec<AgentId>) -> WaitChildrenResult {
    WaitChildrenResult {
        child_ids,
        status: "running".to_string(),
        ready: false,
        terminal: None,
    }
}

fn wait_children_result_terminal(
    child_ids: Vec<AgentId>,
    terminal: ChildTerminalResult,
) -> WaitChildrenResult {
    WaitChildrenResult {
        child_ids,
        status: status_from_terminal(&terminal),
        ready: true,
        terminal: Some(terminal),
    }
}
