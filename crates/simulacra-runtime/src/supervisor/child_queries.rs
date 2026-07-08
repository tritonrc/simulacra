use super::actor::now_ms;
use super::*;

// ---------------------------------------------------------------------------
// Child query / lifecycle handlers
// ---------------------------------------------------------------------------
//
// These AgentSupervisor methods service the JoinChild / ChildStatus /
// WaitChild / WaitChildren / CloseChild payloads and record terminal results
// as child tasks finish. They run under the supervisor's shared mutexes and
// coordinate with waiters registered by the wait_* operations.

impl AgentSupervisor {
    pub(super) fn join_child(&self, child_id: AgentId, result_tx: ChildJoinSender) {
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

    pub(super) fn child_status(&self, child_id: &AgentId) -> Result<ChildStatus, String> {
        let results = lock_mutex(&self.child_results, "child_results");
        let Some(state) = results.get(child_id) else {
            return Err(format!("unknown or closed child_id: {}", child_id.0));
        };
        Ok(status_from_state(state))
    }

    pub(super) fn list_children(&self) -> Vec<ChildRosterEntry> {
        let results = lock_mutex(&self.child_results, "child_results");
        let mut entries = results
            .values()
            .map(roster_entry_from_state)
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.child_id.cmp(&right.child_id));
        entries
    }

    pub(super) fn wait_child(
        &self,
        child_id: AgentId,
        timeout: Duration,
        result_tx: ChildWaitSender,
    ) {
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
    pub(super) fn wait_children(
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

    pub(super) fn close_child(&self, child_id: &AgentId) -> Result<(), String> {
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

// ---------------------------------------------------------------------------
// Result-shaping helpers
// ---------------------------------------------------------------------------

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

fn roster_entry_from_state(state: &ChildRunState) -> ChildRosterEntry {
    let status = status_from_state(state);
    ChildRosterEntry {
        child_id: status.child_id.0,
        agent_type: status.agent_type,
        task: state.metadata.task.clone(),
        status: status.status,
        ready: status.ready,
        elapsed_ms: status.elapsed_ms,
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
