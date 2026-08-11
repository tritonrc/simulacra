#[derive(Clone, Default)]
struct S060SecurityFactory {
    validate_calls: Arc<AtomicUsize>,
    prepare_calls: Arc<AtomicUsize>,
    backend_calls: Arc<AtomicUsize>,
    create_calls: Arc<AtomicUsize>,
    after_calls: Arc<AtomicUsize>,
    release: Arc<Notify>,
    immediate: bool,
}

impl S060SecurityFactory {
    fn counts(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.validate_calls.load(Ordering::SeqCst),
            self.prepare_calls.load(Ordering::SeqCst),
            self.backend_calls.load(Ordering::SeqCst),
            self.create_calls.load(Ordering::SeqCst),
            self.after_calls.load(Ordering::SeqCst),
        )
    }

    fn immediate() -> Self {
        Self {
            immediate: true,
            ..Self::default()
        }
    }
}

impl TaskFactory for S060SecurityFactory {
    fn validate_spawn_config(&self, config: &SpawnConfig) -> Result<(), RuntimeError> {
        self.validate_calls.fetch_add(1, Ordering::SeqCst);
        if config.placement == "worker" {
            Ok(())
        } else {
            Err(RuntimeError::Session("unexpected test placement".into()))
        }
    }

    fn placement_backend(&self, _config: &SpawnConfig) -> simulacra_config::AgentBackend {
        self.backend_calls.fetch_add(1, Ordering::SeqCst);
        simulacra_config::AgentBackend::Native
    }

    fn prepare_spawn_config(&self, _config: &mut SpawnConfig) -> Result<(), RuntimeError> {
        self.prepare_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn create_task(&self, _config: SpawnConfig, _token: CancellationToken) -> BoxTaskFuture {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        if self.immediate {
            return Box::pin(async { Ok(completed_output()) });
        }
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            release.notified().await;
            Ok(completed_output())
        })
    }

    fn after_spawn(
        &self,
        _config: &SpawnConfig,
        _result: &simulacra_runtime::SpawnResult,
    ) -> Result<(), RuntimeError> {
        self.after_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct S060SecurityActivitySink(Arc<Mutex<Vec<simulacra_types::ActivityEvent>>>);

impl simulacra_runtime::ActivitySink for S060SecurityActivitySink {
    fn emit(&self, event: simulacra_types::ActivityEvent) {
        self.0.lock().expect("activity lock").push(event);
    }
}

fn s060_security_supervisor(
    factory: S060SecurityFactory,
    journal: Arc<InMemoryJournalStorage>,
    activity: S060SecurityActivitySink,
) -> AgentSupervisor {
    let mut supervisor = AgentSupervisor::with_task_factory(
        worker_parent_capability(),
        default_budget(),
        Arc::new(factory),
    );
    supervisor.set_journal_storage(journal);
    supervisor.set_activity_sink(Arc::new(activity));
    supervisor
}

fn s060_security_config(child_id: &str, parent_id: &str) -> SpawnConfig {
    spawn_config(
        child_id,
        parent_id,
        CapabilityToken::default(),
        leaf_child_budget(),
        RestartStrategy::LetCrash,
    )
}

fn s060_security_journal_len(journal: &InMemoryJournalStorage, parent_id: &str) -> usize {
    journal
        .read_all(&AgentId(parent_id.into()))
        .expect("parent journal")
        .len()
}

fn s060_security_budget_value(budget: ResourceBudget) -> serde_json::Value {
    serde_json::to_value(budget).expect("budget should serialize")
}

fn s060_assert_unbound_runtime(error: RuntimeError) {
    match error {
        RuntimeError::CapabilityViolation(reason) => {
            assert!(reason.to_lowercase().contains("root"), "{reason}");
            assert!(reason.to_lowercase().contains("unbound"), "{reason}");
            assert!(
                !reason.contains("unknown child_id"),
                "root authentication must precede child lookup: {reason}"
            );
        }
        other => panic!("unbound supervisor must return a typed capability error, got {other:?}"),
    }
}

fn s060_assert_unbound_control<T: std::fmt::Debug>(result: Result<T, String>) {
    let reason = result.expect_err("unbound control must fail closed");
    assert!(reason.to_lowercase().contains("root"), "{reason}");
    assert!(reason.to_lowercase().contains("unbound"), "{reason}");
    assert!(
        !reason.contains("unknown child_id"),
        "root authentication must precede child lookup: {reason}"
    );
}

#[tokio::test]
async fn s060_unbound_direct_spawn_rejects_without_effects_or_first_caller_binding() {
    let factory = S060SecurityFactory::default();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let activity = S060SecurityActivitySink::default();
    let mut supervisor =
        s060_security_supervisor(factory.clone(), Arc::clone(&journal), activity.clone());
    let child_id = "child-00000000000000000000000000000001";
    let caller_id = "arbitrary-first-caller";
    let initial_budget = s060_security_budget_value(supervisor.parent_budget());
    for attempt in 1..=2 {
        let rejected = supervisor
            .spawn_agent(s060_security_config(child_id, caller_id))
            .expect_err("unbound direct spawn must fail closed on every retry");
        s060_assert_unbound_runtime(rejected);
        assert_eq!(
            s060_security_budget_value(supervisor.parent_budget()),
            initial_budget,
            "unbound retry {attempt} changed the complete shared budget account"
        );
    }
    assert_eq!(factory.counts(), (0, 0, 0, 0, 0));
    assert_eq!(s060_security_journal_len(&journal, caller_id), 0);
    assert!(activity.0.lock().expect("activity lock").is_empty());

    supervisor.set_root_agent_id(AgentId("configured-root".into()));
    supervisor
        .spawn_agent(s060_security_config(child_id, "configured-root"))
        .expect("explicit root binding must accept the exact id rejected without effects");
    assert_eq!(factory.create_calls.load(Ordering::SeqCst), 1);
    factory.release.notify_one();
}

#[tokio::test]
async fn s060_unbound_actor_rejects_spawn_and_every_parent_facing_control_without_effects() {
    let factory = S060SecurityFactory::default();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let activity = S060SecurityActivitySink::default();
    let supervisor = Arc::new(s060_security_supervisor(
        factory.clone(),
        Arc::clone(&journal),
        activity.clone(),
    ));
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let actor = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.run_actor_loop(receiver).await })
    };
    let caller = AgentId("unbound-control-caller".into());
    let missing = AgentId("child-ffffffffffffffffffffffffffffffff".into());
    let initial_budget = s060_security_budget_value(supervisor.parent_budget());
    let (host_before_tx, host_before_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("host-inspection".into()),
            payload: SupervisorPayload::InspectChildren(host_before_tx),
        })
        .await
        .expect("host snapshot send");
    let host_before = host_before_rx
        .await
        .expect("host snapshot response")
        .expect("host inspection remains available for zero-effect snapshots");

    let (spawn_tx, spawn_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: caller.clone(),
            payload: SupervisorPayload::Spawn(
                Box::new(s060_security_config(&missing.0, &caller.0)),
                spawn_tx,
            ),
        })
        .await
        .expect("spawn send");
    s060_assert_unbound_runtime(
        spawn_rx
            .await
            .expect("spawn response")
            .expect_err("unbound actor spawn must fail"),
    );

    let (join_tx, join_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: caller.clone(),
            payload: SupervisorPayload::JoinChild(missing.clone(), join_tx),
        })
        .await
        .expect("join send");
    s060_assert_unbound_control(join_rx.await.expect("join response"));

    let (inspect_result_tx, inspect_result_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: caller.clone(),
            payload: SupervisorPayload::InspectChildResult(missing.clone(), inspect_result_tx),
        })
        .await
        .expect("inspect-result send");
    s060_assert_unbound_control(inspect_result_rx.await.expect("inspect-result response"));

    let (status_tx, status_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: caller.clone(),
            payload: SupervisorPayload::ChildStatus(missing.clone(), status_tx),
        })
        .await
        .expect("status send");
    s060_assert_unbound_control(status_rx.await.expect("status response"));

    let (list_tx, list_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: caller.clone(),
            payload: SupervisorPayload::ListChildren(list_tx),
        })
        .await
        .expect("list send");
    s060_assert_unbound_control(list_rx.await.expect("list response"));

    let (wait_tx, wait_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: caller.clone(),
            payload: SupervisorPayload::WaitChild(missing.clone(), Duration::ZERO, wait_tx),
        })
        .await
        .expect("wait send");
    s060_assert_unbound_control(wait_rx.await.expect("wait response"));

    let (wait_many_tx, wait_many_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: caller.clone(),
            payload: SupervisorPayload::WaitChildren(
                vec![missing.clone()],
                Duration::ZERO,
                wait_many_tx,
            ),
        })
        .await
        .expect("wait-many send");
    s060_assert_unbound_control(wait_many_rx.await.expect("wait-many response"));

    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: caller.clone(),
            payload: SupervisorPayload::CloseChild(missing.clone(), close_tx),
        })
        .await
        .expect("close send");
    s060_assert_unbound_control(close_rx.await.expect("close response"));

    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Signal,
            agent_id: caller.clone(),
            payload: SupervisorPayload::CancelChild(missing.clone(), cancel_tx),
        })
        .await
        .expect("cancel send");
    s060_assert_unbound_control(cancel_rx.await.expect("cancel response"));

    let (steer_tx, steer_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: caller.clone(),
            payload: SupervisorPayload::SteerChild(
                missing.clone(),
                "do not enqueue".into(),
                steer_tx,
            ),
        })
        .await
        .expect("steer send");
    s060_assert_unbound_control(steer_rx.await.expect("steer response"));

    let (retry_tx, retry_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: caller.clone(),
            payload: SupervisorPayload::Spawn(
                Box::new(s060_security_config(&missing.0, &caller.0)),
                retry_tx,
            ),
        })
        .await
        .expect("same-caller retry send");
    s060_assert_unbound_runtime(
        retry_rx
            .await
            .expect("same-caller retry response")
            .expect_err("rejected caller must not acquire root authority"),
    );

    let (host_after_tx, host_after_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("host-inspection".into()),
            payload: SupervisorPayload::InspectChildren(host_after_tx),
        })
        .await
        .expect("post-rejection host snapshot send");
    let host_after = host_after_rx
        .await
        .expect("post-rejection host snapshot response")
        .expect("post-rejection host inspection");

    assert_eq!(factory.counts(), (0, 0, 0, 0, 0));
    assert_eq!(
        host_after, host_before,
        "unbound controls mutated child maps"
    );
    assert_eq!(
        s060_security_budget_value(supervisor.parent_budget()),
        initial_budget,
        "unbound controls mutated the complete budget/reservation account"
    );
    assert_eq!(
        s060_security_journal_len(&journal, "unbound-control-caller"),
        0
    );
    assert!(activity.0.lock().expect("activity lock").is_empty());
    drop(sender);
    actor.await.expect("actor stops");

    let mut supervisor = Arc::try_unwrap(supervisor).expect("actor released supervisor");
    supervisor.set_root_agent_id(AgentId("configured-root".into()));
    supervisor
        .spawn_agent(s060_security_config(&missing.0, "configured-root"))
        .expect("explicit root binding must reuse the exact id rejected without effects");
    factory.release.notify_one();
}

#[tokio::test]
async fn s060_direct_duplicate_child_id_rejects_before_factory_and_accepted_effects() {
    let parent_id = "root-direct-duplicate";
    let child_id = "child-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let factory = S060SecurityFactory::default();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let activity = S060SecurityActivitySink::default();
    let mut supervisor =
        s060_security_supervisor(factory.clone(), Arc::clone(&journal), activity.clone());
    supervisor.set_root_agent_id(AgentId(parent_id.into()));
    supervisor
        .spawn_agent(s060_security_config(child_id, parent_id))
        .expect("first id is accepted");
    let counts = factory.counts();
    let journal_len = s060_security_journal_len(&journal, parent_id);
    let activity_len = activity.0.lock().expect("activity lock").len();
    let budget = s060_security_budget_value(supervisor.parent_budget());

    let duplicate = supervisor.spawn_agent(s060_security_config(child_id, parent_id));
    assert!(
        duplicate.is_err(),
        "host-internal duplicate child id must be rejected synchronously"
    );
    assert_eq!(factory.counts(), counts);
    assert_eq!(s060_security_journal_len(&journal, parent_id), journal_len);
    assert_eq!(
        activity.0.lock().expect("activity lock").len(),
        activity_len
    );
    assert_eq!(
        s060_security_budget_value(supervisor.parent_budget()),
        budget
    );
    factory.release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if activity
                .0
                .lock()
                .expect("activity lock")
                .iter()
                .any(|event| matches!(event, simulacra_types::ActivityEvent::ChildFinished { child_id: finished, .. } if finished == child_id))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("original direct child should settle before test teardown");

    let terminal_counts = factory.counts();
    let terminal_journal_len = s060_security_journal_len(&journal, parent_id);
    let terminal_activity_len = activity.0.lock().expect("activity lock").len();
    let terminal_budget = s060_security_budget_value(supervisor.parent_budget());
    assert!(
        supervisor
            .spawn_agent(s060_security_config(child_id, parent_id))
            .is_err(),
        "the direct path must not recycle a terminal opaque id"
    );
    assert_eq!(factory.counts(), terminal_counts);
    assert_eq!(
        s060_security_journal_len(&journal, parent_id),
        terminal_journal_len
    );
    assert_eq!(
        activity.0.lock().expect("activity lock").len(),
        terminal_activity_len
    );
    assert_eq!(
        s060_security_budget_value(supervisor.parent_budget()),
        terminal_budget
    );

    // Direct spawning deliberately has no direct child-control API. Reuse the
    // public actor controls only to prove the direct child remains joinable and
    // can be explicitly closed; the duplicate checks themselves remain direct.
    let supervisor = Arc::new(supervisor);
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    let actor = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.run_actor_loop(receiver).await })
    };
    let child_id = AgentId(child_id.into());
    let parent = AgentId(parent_id.into());
    let (join_tx, join_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: parent.clone(),
            payload: SupervisorPayload::JoinChild(child_id.clone(), join_tx),
        })
        .await
        .expect("terminal direct-child join send");
    assert_eq!(
        join_rx
            .await
            .expect("terminal direct-child join response")
            .expect("original direct child remains joinable after duplicate rejection")
            .child_id,
        child_id
    );
    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: parent,
            payload: SupervisorPayload::CloseChild(child_id.clone(), close_tx),
        })
        .await
        .expect("terminal direct-child close send");
    close_rx
        .await
        .expect("terminal direct-child close response")
        .expect("original direct child remains closable after duplicate rejection");
    drop(sender);
    actor.await.expect("direct-child control actor should stop");

    let mut supervisor = Arc::try_unwrap(supervisor).expect("actor released direct supervisor");
    let closed_counts = factory.counts();
    let closed_journal_len = s060_security_journal_len(&journal, parent_id);
    let closed_activity_len = activity.0.lock().expect("activity lock").len();
    let closed_budget = s060_security_budget_value(supervisor.parent_budget());
    assert!(
        supervisor
            .spawn_agent(s060_security_config(&child_id.0, parent_id))
            .is_err(),
        "an explicitly closed direct child id remains permanently nonrecyclable"
    );
    assert_eq!(factory.counts(), closed_counts);
    assert_eq!(
        s060_security_journal_len(&journal, parent_id),
        closed_journal_len
    );
    assert_eq!(
        activity.0.lock().expect("activity lock").len(),
        closed_activity_len
    );
    assert_eq!(
        s060_security_budget_value(supervisor.parent_budget()),
        closed_budget
    );
}

#[tokio::test]
async fn s060_direct_duplicate_terminal_child_id_preserves_the_first_terminal_lifecycle() {
    let parent_id = "root-direct-terminal-duplicate";
    let child_id = "child-cccccccccccccccccccccccccccccccc";
    let factory = S060SecurityFactory::immediate();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let activity = S060SecurityActivitySink::default();
    let mut supervisor =
        s060_security_supervisor(factory.clone(), Arc::clone(&journal), activity.clone());
    supervisor.set_root_agent_id(AgentId(parent_id.into()));
    supervisor
        .spawn_agent(s060_security_config(child_id, parent_id))
        .expect("the original terminal child is accepted");
    assert!(
        activity.0.lock().expect("activity lock").iter().any(
            |event| matches!(event, simulacra_types::ActivityEvent::ChildFinished { child_id: finished, .. } if finished == child_id)
        ),
        "the original direct child must retain its terminal lifecycle"
    );
    let counts = factory.counts();
    let journal_len = s060_security_journal_len(&journal, parent_id);
    let activity_len = activity.0.lock().expect("activity lock").len();
    let budget = s060_security_budget_value(supervisor.parent_budget());

    assert!(
        supervisor
            .spawn_agent(s060_security_config(child_id, parent_id))
            .is_err(),
        "a terminal opaque id cannot be recycled through the direct API"
    );
    assert_eq!(factory.counts(), counts);
    assert_eq!(s060_security_journal_len(&journal, parent_id), journal_len);
    assert_eq!(
        activity.0.lock().expect("activity lock").len(),
        activity_len
    );
    assert_eq!(
        s060_security_budget_value(supervisor.parent_budget()),
        budget
    );
}

#[cfg(feature = "spawn")]
#[tokio::test]
async fn s060_model_supplied_child_id_is_rejected_before_any_supervisor_dispatch() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let parent_budget = Arc::new(Mutex::new(default_budget()));
    let before_budget =
        s060_security_budget_value(parent_budget.lock().expect("parent budget lock").clone());
    let tool = SpawnAgentTool {
        sender,
        allowed_placements: vec!["worker".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("configured-root".into()),
        parent_budget: Arc::clone(&parent_budget),
        guidance: None,
    };

    let error = tool
        .call(
            serde_json::json!({
                "child_id": "child-model-selected-id",
                "placement": "worker",
                "task": "bounded work",
                "budget": {
                    "max_tokens": 1,
                    "max_turns": 1,
                    "max_cost": "0",
                    "max_sub_agents": 1
                }
            }),
            &worker_parent_capability(),
        )
        .await
        .expect_err("the model-facing spawn contract must reject child_id");
    let error = error.to_string();
    assert!(
        error.contains("child_id"),
        "error names rejected field: {error}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(25), receiver.recv())
            .await
            .is_err(),
        "an invalid model payload must not reach the supervisor"
    );
    assert_eq!(
        s060_security_budget_value(parent_budget.lock().expect("parent budget lock").clone()),
        before_budget,
        "rejecting a model-selected id must not reserve budget"
    );
}

#[tokio::test]
async fn s060_actor_duplicate_preserves_original_running_terminal_and_closed_child_id() {
    let parent_id = AgentId("root-actor-duplicate".into());
    let child_id = AgentId("child-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into());
    let factory = S060SecurityFactory::default();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let activity = S060SecurityActivitySink::default();
    let mut supervisor =
        s060_security_supervisor(factory.clone(), Arc::clone(&journal), activity.clone());
    supervisor.set_root_agent_id(parent_id.clone());
    let supervisor = Arc::new(supervisor);
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let actor = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.run_actor_loop(receiver).await })
    };

    async fn actor_spawn(
        sender: &tokio::sync::mpsc::Sender<SupervisorMessage>,
        parent_id: &AgentId,
        child_id: &AgentId,
    ) -> Result<simulacra_runtime::SpawnAck, RuntimeError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        sender
            .send(SupervisorMessage {
                priority: MessagePriority::Command,
                agent_id: parent_id.clone(),
                payload: SupervisorPayload::Spawn(
                    Box::new(s060_security_config(&child_id.0, &parent_id.0)),
                    tx,
                ),
            })
            .await
            .expect("spawn send");
        rx.await.expect("spawn response")
    }

    actor_spawn(&sender, &parent_id, &child_id)
        .await
        .expect("first id is accepted");
    let counts = factory.counts();
    let journal_len = s060_security_journal_len(&journal, &parent_id.0);
    let activity_len = activity.0.lock().expect("activity lock").len();
    let budget = s060_security_budget_value(supervisor.parent_budget());
    assert!(actor_spawn(&sender, &parent_id, &child_id).await.is_err());
    assert_eq!(factory.counts(), counts);
    assert_eq!(
        s060_security_journal_len(&journal, &parent_id.0),
        journal_len
    );
    assert_eq!(
        activity.0.lock().expect("activity lock").len(),
        activity_len
    );
    assert_eq!(
        s060_security_budget_value(supervisor.parent_budget()),
        budget
    );

    let (status_tx, status_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: parent_id.clone(),
            payload: SupervisorPayload::ChildStatus(child_id.clone(), status_tx),
        })
        .await
        .expect("status send");
    assert_eq!(
        status_rx
            .await
            .expect("status response")
            .expect("original remains usable")
            .status,
        ChildAgentStatus::Running
    );

    factory.release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let (tx, rx) = tokio::sync::oneshot::channel();
            sender
                .send(SupervisorMessage {
                    priority: MessagePriority::Command,
                    agent_id: parent_id.clone(),
                    payload: SupervisorPayload::ChildStatus(child_id.clone(), tx),
                })
                .await
                .expect("terminal poll send");
            if rx
                .await
                .expect("terminal poll response")
                .expect("original remains present")
                .ready
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("original actor child should settle within the bounded test deadline");

    let counts_at_terminal = factory.counts();
    let journal_at_terminal = s060_security_journal_len(&journal, &parent_id.0);
    let activity_at_terminal = activity.0.lock().expect("activity lock").len();
    let budget_at_terminal = s060_security_budget_value(supervisor.parent_budget());
    assert!(
        actor_spawn(&sender, &parent_id, &child_id).await.is_err(),
        "a terminal opaque id remains reserved until the host explicitly closes it"
    );
    assert_eq!(factory.counts(), counts_at_terminal);
    assert_eq!(
        s060_security_journal_len(&journal, &parent_id.0),
        journal_at_terminal
    );
    assert_eq!(
        activity.0.lock().expect("activity lock").len(),
        activity_at_terminal
    );
    assert_eq!(
        s060_security_budget_value(supervisor.parent_budget()),
        budget_at_terminal
    );

    let (join_tx, join_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: parent_id.clone(),
            payload: SupervisorPayload::JoinChild(child_id.clone(), join_tx),
        })
        .await
        .expect("terminal join send");
    assert_eq!(
        join_rx
            .await
            .expect("terminal join response")
            .expect("duplicate rejection must not corrupt the original terminal result")
            .child_id,
        child_id
    );

    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: parent_id.clone(),
            payload: SupervisorPayload::CloseChild(child_id.clone(), close_tx),
        })
        .await
        .expect("close send");
    close_rx
        .await
        .expect("close response")
        .expect("terminal original closes");
    let counts_after_close = factory.counts();
    let journal_after_close = s060_security_journal_len(&journal, &parent_id.0);
    let activity_after_close = activity.0.lock().expect("activity lock").len();
    let budget_after_close = s060_security_budget_value(supervisor.parent_budget());
    assert!(
        actor_spawn(&sender, &parent_id, &child_id).await.is_err(),
        "a closed opaque id remains closed and cannot be recycled"
    );
    assert_eq!(factory.counts(), counts_after_close);
    assert_eq!(
        s060_security_journal_len(&journal, &parent_id.0),
        journal_after_close
    );
    assert_eq!(
        activity.0.lock().expect("activity lock").len(),
        activity_after_close
    );
    assert_eq!(
        s060_security_budget_value(supervisor.parent_budget()),
        budget_after_close
    );

    drop(sender);
    actor.await.expect("actor stops");
}
