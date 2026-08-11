// S060/S018 security remediation REDs. These tests stay on the real
// SpawnAgentTool -> supervisor actor -> TaskFactory path. The factory controls
// only when a child becomes terminal; it does not fake authorization or budget
// decisions owned by the supervisor.

#[derive(Clone, Default)]
struct S060BudgetGateFactory {
    inner: Arc<S060BudgetGateFactoryInner>,
}

#[derive(Default)]
struct S060BudgetGateFactoryInner {
    pending: Mutex<
        HashMap<AgentId, tokio::sync::oneshot::Sender<Result<AgentLoopOutput, RuntimeError>>>,
    >,
    started: Mutex<Vec<SpawnConfig>>,
    started_notify: Notify,
    prepare_calls: AtomicUsize,
    prepare_failures_remaining: AtomicUsize,
}

impl S060BudgetGateFactory {
    async fn wait_for_started(&self, expected: usize) {
        loop {
            let notified = self.inner.started_notify.notified();
            if self.inner.started.lock().expect("started lock").len() >= expected {
                return;
            }
            tokio::time::timeout(Duration::from_secs(2), notified)
                .await
                .expect("expected child factory invocation before timeout");
        }
    }

    fn finish(&self, child_id: &AgentId, output: AgentLoopOutput) {
        self.finish_result(child_id, Ok(output));
    }

    fn fail(&self, child_id: &AgentId, reason: &str) {
        self.finish_result(child_id, Err(RuntimeError::Session(reason.to_string())));
    }

    fn finish_result(&self, child_id: &AgentId, result: Result<AgentLoopOutput, RuntimeError>) {
        self.inner
            .pending
            .lock()
            .expect("pending lock")
            .remove(child_id)
            .unwrap_or_else(|| panic!("expected pending child {}", child_id.0))
            .send(result)
            .unwrap_or_else(|_| panic!("pending child {} should receive output", child_id.0));
    }

    fn started(&self) -> Vec<SpawnConfig> {
        self.inner.started.lock().expect("started lock").clone()
    }

    fn fail_next_prepare(&self) {
        self.inner
            .prepare_failures_remaining
            .store(1, Ordering::SeqCst);
    }

    fn prepare_calls(&self) -> usize {
        self.inner.prepare_calls.load(Ordering::SeqCst)
    }

    fn finish_all(&self) {
        let pending = std::mem::take(&mut *self.inner.pending.lock().expect("pending lock"));
        for (_, sender) in pending {
            let _ = sender.send(Ok(s060_budget_usage_output(0, 0, Decimal::ZERO)));
        }
    }
}

impl TaskFactory for S060BudgetGateFactory {
    fn validate_spawn_config(&self, config: &SpawnConfig) -> Result<(), RuntimeError> {
        if matches!(config.placement.as_str(), "worker" | "leaf") {
            Ok(())
        } else {
            Err(RuntimeError::Session(format!(
                "unknown child placement {:?}; available placements: leaf, worker",
                config.placement
            )))
        }
    }

    fn prepare_spawn_config(&self, _config: &mut SpawnConfig) -> Result<(), RuntimeError> {
        self.inner.prepare_calls.fetch_add(1, Ordering::SeqCst);
        if self
            .inner
            .prepare_failures_remaining
            .swap(0, Ordering::SeqCst)
            > 0
        {
            return Err(RuntimeError::Session(
                "injected prepare_spawn_config failure".into(),
            ));
        }
        Ok(())
    }

    fn create_task(&self, config: SpawnConfig, cancellation: CancellationToken) -> BoxTaskFuture {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.inner
            .pending
            .lock()
            .expect("pending lock")
            .insert(config.agent_id.clone(), sender);
        self.inner
            .started
            .lock()
            .expect("started lock")
            .push(config);
        self.inner.started_notify.notify_waiters();

        Box::pin(async move {
            let mut receiver = receiver;
            loop {
                if cancellation.is_cancelled() {
                    return Ok(AgentLoopOutput {
                        exit_reason: ExitReason::Cancelled,
                        ..s060_budget_usage_output(0, 0, Decimal::ZERO)
                    });
                }
                match tokio::time::timeout(Duration::from_millis(2), &mut receiver).await {
                    Ok(Ok(result)) => return result,
                    Ok(Err(_)) => {
                        return Err(RuntimeError::Session(
                            "budget gate dropped before child completion".into(),
                        ));
                    }
                    Err(_) => {}
                }
            }
        })
    }
}

#[derive(Default)]
struct S060BudgetActivityRecorder {
    events: Mutex<Vec<simulacra_types::ActivityEvent>>,
}

impl simulacra_runtime::ActivitySink for S060BudgetActivityRecorder {
    fn emit(&self, event: simulacra_types::ActivityEvent) {
        self.events.lock().expect("activity lock").push(event);
    }
}

impl S060BudgetActivityRecorder {
    fn events(&self) -> Vec<simulacra_types::ActivityEvent> {
        self.events.lock().expect("activity lock").clone()
    }
}

struct S060FailFirstSpawnJournal {
    inner: Arc<InMemoryJournalStorage>,
    failed_spawn_appends: AtomicUsize,
}

impl S060FailFirstSpawnJournal {
    fn new(inner: Arc<InMemoryJournalStorage>) -> Self {
        Self {
            inner,
            failed_spawn_appends: AtomicUsize::new(0),
        }
    }
}

impl JournalStorage for S060FailFirstSpawnJournal {
    fn append(&self, entry: JournalEntry) -> Result<(), simulacra_types::JournalError> {
        if matches!(&entry.entry, JournalEntryKind::SubAgentSpawned { .. })
            && self.failed_spawn_appends.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Err(simulacra_types::JournalError::Storage(
                "injected first SubAgentSpawned append failure".into(),
            ));
        }
        self.inner.append(entry)
    }

    fn read_all(
        &self,
        agent_id: &AgentId,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        self.inner.read_all(agent_id)
    }

    fn query_token_usage(
        &self,
        agent_id: &AgentId,
    ) -> Result<TokenUsage, simulacra_types::JournalError> {
        self.inner.query_token_usage(agent_id)
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        after_entry: usize,
        data: simulacra_types::CheckpointData,
    ) -> Result<(), simulacra_types::JournalError> {
        self.inner.save_checkpoint(agent_id, after_entry, data)
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        self.inner.fork_from(agent_id, checkpoint_idx)
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        self.inner.read_from(agent_id, start_index)
    }
}

fn s060_budget_usage_output(tokens: u64, turns: u32, cost: Decimal) -> AgentLoopOutput {
    AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![],
        token_usage: TokenUsage {
            input_tokens: tokens,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
        },
        reported_tool_uses: None,
        used_turns: turns,
        used_cost: cost,
    }
}

fn s060_budget_security_tool(
    sender: &tokio::sync::mpsc::Sender<SupervisorMessage>,
    caller_id: &str,
    parent_budget: Arc<Mutex<ResourceBudget>>,
    allowed_placements: &[&str],
) -> SpawnAgentTool {
    SpawnAgentTool {
        sender: sender.clone(),
        allowed_placements: allowed_placements
            .iter()
            .map(|placement| (*placement).to_string())
            .collect(),
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId(caller_id.into()),
        parent_budget,
        guidance: None,
    }
}

fn s060_budget_security_args(
    placement: &str,
    tokens: u64,
    turns: u32,
    cost: Decimal,
    sub_agents: u32,
    descendant_placements: Option<&[&str]>,
) -> serde_json::Value {
    let mut arguments = serde_json::json!({
        "placement": placement,
        "task": format!("bounded task for {placement}"),
        "budget": {
            "max_tokens": tokens,
            "max_turns": turns,
            "max_cost": cost.to_string(),
            "max_sub_agents": sub_agents
        }
    });
    if let Some(placements) = descendant_placements {
        arguments["capabilities"] = serde_json::json!({
            "spawn_placements": placements
        });
    }
    arguments
}

fn s060_child_id(acknowledgement: &serde_json::Value) -> AgentId {
    AgentId(
        acknowledgement["child_id"]
            .as_str()
            .expect("accepted spawn acknowledgement should contain child_id")
            .to_string(),
    )
}

fn s060_budget_security_capability(placements: &[&str]) -> CapabilityToken {
    CapabilityToken {
        spawn_placements: placements
            .iter()
            .map(|placement| (*placement).to_string())
            .collect(),
        ..CapabilityToken::default()
    }
}

async fn s060_join_budget_child(
    sender: &tokio::sync::mpsc::Sender<SupervisorMessage>,
    caller_id: &AgentId,
    child_id: &AgentId,
) {
    JoinChildAgentTool {
        sender: sender.clone(),
        caller_id: caller_id.clone(),
    }
    .call(
        serde_json::json!({ "child_id": child_id.0 }),
        &CapabilityToken::default(),
    )
    .await
    .expect("owner should join completed child");
}

#[tokio::test]
async fn s060_child_sub_agent_limit_is_authoritative_when_root_has_spare_capacity() {
    let root_id = AgentId("budget-root".into());
    let root_capability = s060_budget_security_capability(&["worker", "leaf"]);
    let root_budget = Arc::new(Mutex::new(ResourceBudget::new(
        1_000,
        100,
        Decimal::new(100, 0),
        8,
    )));
    let factory = Arc::new(S060BudgetGateFactory::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        root_capability.clone(),
        Arc::clone(&root_budget),
        Arc::clone(&factory) as Arc<dyn TaskFactory>,
    );
    let journal = Arc::new(InMemoryJournalStorage::new());
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    supervisor.set_root_agent_id(root_id.clone());
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });

    let root_tool =
        s060_budget_security_tool(&sender, &root_id.0, Arc::clone(&root_budget), &["worker"]);
    let child_ack = root_tool
        .call(
            s060_budget_security_args("worker", 100, 10, Decimal::new(10, 0), 1, Some(&["leaf"])),
            &root_capability,
        )
        .await
        .expect("root should be allowed to create the worker");
    let child_id = s060_child_id(&child_ack);
    factory.wait_for_started(1).await;

    // Deliberately give the proxy generous headroom. The actor/supervisor must
    // remain authoritative and resolve the accepted worker's own budget.
    let child_proxy_budget = Arc::new(Mutex::new(ResourceBudget::new(
        1_000,
        100,
        Decimal::new(100, 0),
        8,
    )));
    let child_tool = Arc::new(s060_budget_security_tool(
        &sender,
        &child_id.0,
        child_proxy_budget,
        &["leaf"],
    ));
    let child_capability = s060_budget_security_capability(&["leaf"]);
    let barrier = Arc::new(tokio::sync::Barrier::new(33));
    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let child_tool = Arc::clone(&child_tool);
        let child_capability = child_capability.clone();
        let barrier = Arc::clone(&barrier);
        calls.spawn(async move {
            barrier.wait().await;
            child_tool
                .call(
                    s060_budget_security_args("leaf", 1, 1, Decimal::new(1, 1), 1, None),
                    &child_capability,
                )
                .await
        });
    }
    barrier.wait().await;
    let mut outcomes = Vec::new();
    while let Some(outcome) = calls.join_next().await {
        outcomes.push(outcome.expect("parallel descendant spawn must not panic"));
    }

    let accepted = outcomes.iter().filter(|result| result.is_ok()).count();
    let failures = outcomes
        .iter()
        .filter_map(|result| result.as_ref().err())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let root_used_sub_agents = root_budget
        .lock()
        .expect("root budget lock")
        .used_sub_agents;
    let started = factory.started();
    let worker_spawned_leaf_entries = journal
        .read_all(&child_id)
        .expect("worker journal should remain readable")
        .into_iter()
        .filter(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::SubAgentSpawned { placement, .. } if placement == "leaf"
            )
        })
        .count();

    factory.finish_all();
    drop(child_tool);
    drop(root_tool);
    drop(sender);
    actor.await.expect("supervisor actor should stop cleanly");

    assert_eq!(
        accepted, 1,
        "the worker's max_sub_agents=1 must admit exactly one direct descendant"
    );
    assert_eq!(
        failures.len(),
        31,
        "all 31 losing calls must fail reservation"
    );
    assert!(
        failures.iter().all(|error| {
            error.contains("max_sub_agents requested 1")
                && error.contains("immediate parent remaining 0")
                && error.contains("limit 1")
        }),
        "every losing call must name the request and immediate-parent limit: {failures:?}"
    );
    assert_eq!(
        started.len(),
        2,
        "the factory must start exactly the worker and its one accepted leaf"
    );
    assert_eq!(started[0].agent_id, child_id);
    assert_eq!(started[0].placement, "worker");
    assert_eq!(started[1].parent_id, child_id);
    assert_eq!(started[1].placement, "leaf");
    assert_eq!(started[1].budget.max_tokens, 1);
    assert_eq!(started[1].budget.max_turns, 1);
    assert_eq!(started[1].budget.max_cost, Decimal::new(1, 1));
    assert_eq!(started[1].budget.max_sub_agents, 1);
    assert_eq!(
        worker_spawned_leaf_entries, 1,
        "the worker journal must contain exactly one accepted leaf spawn"
    );
    assert_eq!(
        root_used_sub_agents, 1,
        "the root's direct-child count must remain one; the accepted leaf belongs to the immediate worker budget"
    );
}

#[tokio::test]
async fn s060_fully_reserved_root_does_not_consume_child_headroom_and_child_usage_is_live() {
    let root_id = AgentId("fully-reserved-root".into());
    let capability = s060_budget_security_capability(&["worker", "leaf"]);
    let root_budget = Arc::new(Mutex::new(ResourceBudget::new(
        10,
        10,
        Decimal::new(10, 0),
        0,
    )));
    let factory = Arc::new(S060BudgetGateFactory::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&root_budget),
        Arc::clone(&factory) as Arc<dyn TaskFactory>,
    );
    supervisor.set_journal_storage(Arc::new(InMemoryJournalStorage::new()));
    supervisor.set_root_agent_id(root_id.clone());
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });

    let root_tool =
        s060_budget_security_tool(&sender, &root_id.0, Arc::clone(&root_budget), &["worker"]);
    let worker_ack = root_tool
        .call(
            s060_budget_security_args("worker", 10, 10, Decimal::new(10, 0), 3, Some(&["leaf"])),
            &capability,
        )
        .await
        .expect("worker may reserve all of the root's token/turn/cost headroom");
    let worker_id = s060_child_id(&worker_ack);
    factory.wait_for_started(1).await;

    // An unlimited proxy prevents the model-facing preflight from deciding the
    // hierarchy. The supervisor must resolve the worker's live budget.
    let child_proxy = Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0)));
    let worker_tool = s060_budget_security_tool(&sender, &worker_id.0, child_proxy, &["leaf"]);
    let worker_capability = s060_budget_security_capability(&["leaf"]);
    let first_ack = worker_tool
        .call(
            s060_budget_security_args("leaf", 4, 4, Decimal::new(4, 0), 1, None),
            &worker_capability,
        )
        .await
        .expect("root reservation must not consume the immediate child's own headroom");
    let first_id = s060_child_id(&first_ack);
    factory.wait_for_started(2).await;
    factory.finish(
        &first_id,
        s060_budget_usage_output(4, 4, Decimal::new(4, 0)),
    );
    s060_join_budget_child(&sender, &worker_id, &first_id).await;

    let boundary_ack = worker_tool
        .call(
            s060_budget_security_args("leaf", 6, 6, Decimal::new(6, 0), 1, None),
            &worker_capability,
        )
        .await
        .expect("request at the worker's live remaining boundary must be accepted");
    let boundary_id = s060_child_id(&boundary_ack);
    factory.wait_for_started(3).await;
    factory.finish(&boundary_id, s060_budget_usage_output(0, 0, Decimal::ZERO));
    s060_join_budget_child(&sender, &worker_id, &boundary_id).await;

    let second = worker_tool
        .call(
            s060_budget_security_args("leaf", 7, 7, Decimal::new(7, 0), 1, None),
            &worker_capability,
        )
        .await
        .expect_err("completed leaf usage must reduce its immediate parent's live headroom");
    let error = second.to_string();
    assert!(
        error.contains("max_tokens")
            && error.contains("requested 7")
            && error.contains("immediate parent")
            && error.contains("remaining 6"),
        "rejection must expose requested and immediate-parent values: {error}"
    );

    let started = factory.started();
    assert_eq!(
        started.len(),
        3,
        "rejected over-boundary leaf must not reach factory"
    );
    assert_eq!(started[0].budget.max_tokens, 10);
    assert_eq!(started[1].parent_id, worker_id);
    assert_eq!(started[1].budget.max_tokens, 4);
    assert_eq!(started[2].parent_id, worker_id);
    assert_eq!(started[2].budget.max_tokens, 6);

    factory.finish_all();
    drop(worker_tool);
    drop(root_tool);
    drop(sender);
    actor.await.expect("supervisor actor should stop cleanly");
}

#[tokio::test]
async fn s060_zero_budget_semantics_use_each_immediate_parent_dimension_without_effects_on_reject()
{
    let cases = [
        (
            "max_tokens",
            (10, 10, Decimal::new(10, 0), 2),
            (0, 1, Decimal::new(1, 0), 1),
            "10",
        ),
        (
            "max_turns",
            (10, 2, Decimal::new(10, 0), 2),
            (1, 0, Decimal::new(1, 0), 1),
            "2",
        ),
        (
            "max_cost",
            (10, 10, Decimal::new(2, 0), 2),
            (1, 1, Decimal::ZERO, 1),
            "2",
        ),
        (
            "max_sub_agents",
            (10, 10, Decimal::new(10, 0), 2),
            (1, 1, Decimal::new(1, 0), 0),
            "2",
        ),
    ];

    for (field, finite_worker, zero_leaf, finite_limit) in cases {
        for parent_is_unlimited in [false, true] {
            let root_id = AgentId(format!("zero-{field}-{parent_is_unlimited}-root"));
            let capability = s060_budget_security_capability(&["worker", "leaf"]);
            let root_budget = Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0)));
            let factory = Arc::new(S060BudgetGateFactory::default());
            let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
                capability.clone(),
                Arc::clone(&root_budget),
                Arc::clone(&factory) as Arc<dyn TaskFactory>,
            );
            let journal = Arc::new(InMemoryJournalStorage::new());
            supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
            supervisor.set_root_agent_id(root_id.clone());
            let (sender, receiver) = tokio::sync::mpsc::channel(16);
            let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
            let root_tool = s060_budget_security_tool(
                &sender,
                &root_id.0,
                Arc::clone(&root_budget),
                &["worker"],
            );

            let mut worker = finite_worker;
            if parent_is_unlimited {
                match field {
                    "max_tokens" => worker.0 = 0,
                    "max_turns" => worker.1 = 0,
                    "max_cost" => worker.2 = Decimal::ZERO,
                    "max_sub_agents" => worker.3 = 0,
                    _ => unreachable!("table only contains budget fields"),
                }
            }
            let worker_ack = root_tool
                .call(
                    s060_budget_security_args(
                        "worker",
                        worker.0,
                        worker.1,
                        worker.2,
                        worker.3,
                        Some(&["leaf"]),
                    ),
                    &capability,
                )
                .await
                .expect("unlimited root should accept the worker budget");
            let worker_id = s060_child_id(&worker_ack);
            factory.wait_for_started(1).await;

            let worker_tool = s060_budget_security_tool(
                &sender,
                &worker_id.0,
                Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
                &["leaf"],
            );
            let worker_capability = s060_budget_security_capability(&["leaf"]);
            let outcome = worker_tool
                .call(
                    s060_budget_security_args(
                        "leaf",
                        zero_leaf.0,
                        zero_leaf.1,
                        zero_leaf.2,
                        zero_leaf.3,
                        None,
                    ),
                    &worker_capability,
                )
                .await;

            if parent_is_unlimited {
                let acknowledgement = outcome.unwrap_or_else(|error| {
                    panic!(
                        "zero {field} must be accepted under unlimited immediate parent: {error}"
                    )
                });
                assert_eq!(acknowledgement["status"], "running");
                factory.wait_for_started(2).await;
                let started = factory.started();
                assert_eq!(started.len(), 2);
                assert_eq!(started[1].parent_id, worker_id);
                assert_eq!(started[1].budget.max_tokens, zero_leaf.0);
                assert_eq!(started[1].budget.max_turns, zero_leaf.1);
                assert_eq!(started[1].budget.max_cost, zero_leaf.2);
                assert_eq!(started[1].budget.max_sub_agents, zero_leaf.3);
                assert_eq!(
                    journal
                        .read_all(&worker_id)
                        .expect("worker journal")
                        .into_iter()
                        .filter(|entry| matches!(
                            entry.entry,
                            JournalEntryKind::SubAgentSpawned { .. }
                        ))
                        .count(),
                    1,
                    "accepted unlimited request must have exactly one worker-owned spawn entry"
                );
            } else {
                let error = outcome
                    .expect_err("zero request under finite immediate parent must be rejected")
                    .to_string();
                assert!(
                    error.contains(&format!("{field} requested 0"))
                        && error.contains("unlimited")
                        && error.contains(&format!("immediate parent limit {finite_limit}")),
                    "zero rejection must name the request and finite immediate-parent limit: {error}"
                );
                assert_eq!(
                    factory.started().len(),
                    1,
                    "rejected zero request must not reach the factory"
                );
                assert!(
                    journal
                        .read_all(&worker_id)
                        .expect("worker journal")
                        .is_empty(),
                    "rejected zero request must not journal an accepted spawn"
                );
            }
            assert_eq!(
                root_budget
                    .lock()
                    .expect("root budget lock")
                    .used_sub_agents,
                1,
                "descendant acceptance or rejection must not change root direct-child usage"
            );

            factory.finish_all();
            drop(worker_tool);
            drop(root_tool);
            drop(sender);
            actor.await.expect("supervisor actor should stop cleanly");
        }
    }
}

#[tokio::test]
async fn s060_descendant_token_turn_and_cost_limits_resolve_against_immediate_parent() {
    let root_id = AgentId("asymmetric-budget-root".into());
    let root_capability = s060_budget_security_capability(&["worker", "leaf"]);
    let root_budget = Arc::new(Mutex::new(ResourceBudget::new(
        10_000,
        1_000,
        Decimal::new(1_000, 0),
        0,
    )));
    let factory = Arc::new(S060BudgetGateFactory::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        root_capability.clone(),
        Arc::clone(&root_budget),
        Arc::clone(&factory) as Arc<dyn TaskFactory>,
    );
    supervisor.set_journal_storage(Arc::new(InMemoryJournalStorage::new()));
    supervisor.set_root_agent_id(root_id.clone());
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    let root_tool =
        s060_budget_security_tool(&sender, &root_id.0, Arc::clone(&root_budget), &["worker"]);
    let child_ack = root_tool
        .call(
            s060_budget_security_args("worker", 10, 2, Decimal::new(1, 0), 4, Some(&["leaf"])),
            &root_capability,
        )
        .await
        .expect("root should be allowed to create the bounded worker");
    let child_id = s060_child_id(&child_ack);
    factory.wait_for_started(1).await;

    // The call-site proxy is intentionally backed by the root's larger budget.
    // Each request therefore reaches the actor and proves the supervisor does
    // not accidentally reuse root limits for a descendant.
    let child_tool =
        s060_budget_security_tool(&sender, &child_id.0, Arc::clone(&root_budget), &["leaf"]);
    let child_capability = s060_budget_security_capability(&["leaf"]);
    let cases = [
        (11, 1, Decimal::new(1, 1), "max_tokens", "11", "10"),
        (1, 3, Decimal::new(1, 1), "max_turns", "3", "2"),
        (1, 1, Decimal::new(101, 2), "max_cost", "1.01", "1"),
    ];
    let mut outcomes = Vec::new();
    for (tokens, turns, cost, field, requested, limit) in cases {
        outcomes.push((
            field,
            requested,
            limit,
            child_tool
                .call(
                    s060_budget_security_args("leaf", tokens, turns, cost, 1, None),
                    &child_capability,
                )
                .await,
        ));
    }

    factory.finish_all();
    drop(child_tool);
    drop(root_tool);
    drop(sender);
    actor.await.expect("supervisor actor should stop cleanly");

    for (field, requested, limit, outcome) in outcomes {
        let error = outcome.expect_err("request above immediate parent limit must be rejected");
        let error = error.to_string();
        assert!(
            error.contains(&format!("{field} requested {requested}"))
                && error.contains(&format!("immediate parent remaining {limit}")),
            "budget rejection should name {field}, request {requested}, and immediate-parent limit {limit}: {error}"
        );
    }
}

#[tokio::test]
async fn s060_parallel_sibling_reservations_cannot_overcommit_tokens_turns_or_cost() {
    let cases = [
        (
            10,
            100,
            Decimal::new(100, 0),
            6,
            1,
            Decimal::new(1, 0),
            "max_tokens",
        ),
        (
            100,
            10,
            Decimal::new(100, 0),
            1,
            6,
            Decimal::new(1, 0),
            "max_turns",
        ),
        (
            100,
            100,
            Decimal::new(10, 0),
            1,
            1,
            Decimal::new(6, 0),
            "max_cost",
        ),
    ];

    for (max_tokens, max_turns, max_cost, tokens, turns, cost, field) in cases {
        let root_id = AgentId(format!("atomic-{field}-root"));
        let capability = s060_budget_security_capability(&["worker"]);
        let budget = Arc::new(Mutex::new(ResourceBudget::new(
            max_tokens, max_turns, max_cost, 0,
        )));
        let factory = Arc::new(S060BudgetGateFactory::default());
        let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
            capability.clone(),
            Arc::clone(&budget),
            Arc::clone(&factory) as Arc<dyn TaskFactory>,
        );
        supervisor.set_journal_storage(Arc::new(InMemoryJournalStorage::new()));
        supervisor.set_root_agent_id(root_id.clone());
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
        let tool = Arc::new(s060_budget_security_tool(
            &sender,
            &root_id.0,
            Arc::clone(&budget),
            &["worker"],
        ));

        let barrier = Arc::new(tokio::sync::Barrier::new(9));
        let mut calls = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let tool = Arc::clone(&tool);
            let capability = capability.clone();
            let barrier = Arc::clone(&barrier);
            calls.spawn(async move {
                barrier.wait().await;
                tool.call(
                    s060_budget_security_args("worker", tokens, turns, cost, 0, None),
                    &capability,
                )
                .await
            });
        }
        barrier.wait().await;
        let mut outcomes = Vec::new();
        while let Some(result) = calls.join_next().await {
            outcomes.push(result.expect("parallel spawn call should not panic"));
        }
        let accepted = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
        let rejection_text = outcomes
            .iter()
            .filter_map(|outcome| outcome.as_ref().err())
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let started = factory.started();

        factory.finish_all();
        drop(tool);
        drop(sender);
        actor.await.expect("supervisor actor should stop cleanly");

        assert_eq!(
            accepted, 1,
            "atomic {field} reservations must prevent concurrent aggregate overcommit"
        );
        assert_eq!(
            started.len(),
            1,
            "only the winning reservation reaches factory"
        );
        assert_eq!(started[0].budget.max_tokens, tokens);
        assert_eq!(started[0].budget.max_turns, turns);
        assert_eq!(started[0].budget.max_cost, cost);
        assert!(
            rejection_text.iter().all(|error| {
                error.contains(&format!("{field} requested 6"))
                    && error.contains("immediate parent remaining 4")
            }),
            "every losing {field} reservation should name request 6 and immediate-parent remaining 4: {rejection_text:?}"
        );
    }
}

#[tokio::test]
async fn s060_thirty_two_call_batches_preserve_shared_budget_and_durable_spawn_boundary() {
    let root_id = AgentId("s060-thirty-two-root".into());
    let capability = s060_budget_security_capability(&["worker"]);
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        1_000,
        100,
        Decimal::new(100, 0),
        1,
    )));
    let journal = Arc::new(InMemoryJournalStorage::new());
    let factory = Arc::new(S060BudgetGateFactory::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&budget),
        Arc::clone(&factory) as Arc<dyn TaskFactory>,
    );
    supervisor.set_root_agent_id(root_id.clone());
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    let tool = Arc::new(s060_budget_security_tool(
        &sender,
        &root_id.0,
        Arc::clone(&budget),
        &["worker"],
    ));

    let barrier = Arc::new(tokio::sync::Barrier::new(33));
    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let tool = Arc::clone(&tool);
        let capability = capability.clone();
        let barrier = Arc::clone(&barrier);
        calls.spawn(async move {
            barrier.wait().await;
            tool.call(
                s060_budget_security_args("worker", 10, 1, Decimal::ONE, 1, None),
                &capability,
            )
            .await
        });
    }
    barrier.wait().await;
    let mut outcomes = Vec::new();
    while let Some(outcome) = calls.join_next().await {
        outcomes.push(outcome.expect("concurrent spawn caller should not panic"));
    }
    let acknowledgements = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_ref().ok())
        .collect::<Vec<_>>();
    assert_eq!(
        acknowledgements.len(),
        1,
        "only one shared reservation may be accepted"
    );
    assert_eq!(
        factory.started().len(),
        1,
        "only the accepted call reaches construction"
    );
    let spawned = journal
        .read_all(&root_id)
        .expect("accepted root journal should be readable")
        .into_iter()
        .filter_map(|entry| match entry.entry {
            JournalEntryKind::SubAgentSpawned { child_id, .. } => Some(child_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        spawned.len(),
        1,
        "32 calls must produce one durable accepted spawn"
    );
    assert_eq!(
        spawned[0].0,
        acknowledgements[0]["child_id"]
            .as_str()
            .expect("acknowledgement id"),
        "the sole journaled spawn belongs to the sole acknowledgement"
    );
    assert_eq!(
        budget.lock().expect("shared budget").used_sub_agents,
        1,
        "the losing calls must not reserve shared child capacity"
    );

    let accepted_id = s060_child_id(acknowledgements[0]);
    factory.finish(&accepted_id, s060_budget_usage_output(0, 0, Decimal::ZERO));
    s060_join_budget_child(&sender, &root_id, &accepted_id).await;
    drop(tool);
    drop(sender);
    actor
        .await
        .expect("concurrent-reservation actor should stop");

    let root_id = AgentId("s060-invalid-before-followup-root".into());
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        1_000,
        100,
        Decimal::new(100, 0),
        1,
    )));
    let initial_budget = serde_json::to_value(budget.lock().expect("shared budget").clone())
        .expect("shared budget should serialize");
    let journal = Arc::new(InMemoryJournalStorage::new());
    let factory = Arc::new(S060BudgetGateFactory::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&budget),
        Arc::clone(&factory) as Arc<dyn TaskFactory>,
    );
    supervisor.set_root_agent_id(root_id.clone());
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    let tool = Arc::new(s060_budget_security_tool(
        &sender,
        &root_id.0,
        Arc::clone(&budget),
        &["worker"],
    ));

    let mut invalid_calls = tokio::task::JoinSet::new();
    for index in 0..32 {
        let tool = Arc::clone(&tool);
        let capability = capability.clone();
        invalid_calls.spawn(async move {
            let placement = if index % 2 == 0 {
                "unknown"
            } else {
                "forbidden"
            };
            tool.call(
                s060_budget_security_args(placement, 10, 1, Decimal::ONE, 1, None),
                &capability,
            )
            .await
        });
    }
    while let Some(outcome) = invalid_calls.join_next().await {
        assert!(
            outcome
                .expect("invalid spawn caller should not panic")
                .is_err(),
            "unknown and unauthorized placements must be rejected"
        );
    }
    assert!(
        factory.started().is_empty(),
        "invalid calls must not construct children"
    );
    assert!(
        journal
            .read_all(&root_id)
            .expect("invalid root journal")
            .is_empty(),
        "invalid calls must not append lifecycle entries"
    );
    assert_eq!(
        serde_json::to_value(budget.lock().expect("shared budget").clone())
            .expect("shared budget should serialize"),
        initial_budget,
        "invalid calls must leave usage unchanged before the valid follow-up"
    );

    let acknowledgement = tool
        .call(
            s060_budget_security_args("worker", 10, 1, Decimal::ONE, 1, None),
            &capability,
        )
        .await
        .expect("unchanged shared budget must accept the valid follow-up");
    assert_eq!(factory.started().len(), 1);
    assert_eq!(
        journal
            .read_all(&root_id)
            .expect("valid follow-up journal")
            .iter()
            .filter(|entry| matches!(entry.entry, JournalEntryKind::SubAgentSpawned { .. }))
            .count(),
        1,
        "only the valid follow-up creates a durable accepted spawn"
    );
    let child_id = s060_child_id(&acknowledgement);
    factory.finish(&child_id, s060_budget_usage_output(0, 0, Decimal::ZERO));
    s060_join_budget_child(&sender, &root_id, &child_id).await;
    drop(tool);
    drop(sender);
    actor.await.expect("invalid-batch actor should stop");
}

#[tokio::test]
async fn s060_prepare_failure_leaves_no_reservation_or_accepted_state_and_full_headroom_reusable() {
    let root_id = AgentId("prepare-rollback-root".into());
    let capability = s060_budget_security_capability(&["worker"]);
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        100,
        10,
        Decimal::new(10, 0),
        1,
    )));
    let factory = Arc::new(S060BudgetGateFactory::default());
    factory.fail_next_prepare();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let activity = Arc::new(S060BudgetActivityRecorder::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&budget),
        Arc::clone(&factory) as Arc<dyn TaskFactory>,
    );
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    supervisor.set_root_agent_id(root_id.clone());
    supervisor.set_activity_sink(Arc::clone(&activity) as Arc<dyn simulacra_runtime::ActivitySink>);
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    let tool = s060_budget_security_tool(&sender, &root_id.0, Arc::clone(&budget), &["worker"]);
    let roster_tool = ListChildAgentTool {
        sender: sender.clone(),
        caller_id: root_id.clone(),
    };

    let error = tool
        .call(
            s060_budget_security_args("worker", 100, 10, Decimal::new(10, 0), 1, None),
            &capability,
        )
        .await
        .expect_err("injected factory preparation failure must reject spawn")
        .to_string();
    assert!(error.contains("injected prepare_spawn_config failure"));
    assert_eq!(factory.prepare_calls(), 1);
    assert!(
        factory.started().is_empty(),
        "child construction must not run"
    );
    assert!(activity.events().is_empty(), "rejection emits no activity");
    assert!(
        journal.read_all(&root_id).expect("root journal").is_empty(),
        "rejection emits no lifecycle journal entry"
    );
    assert_eq!(
        roster_tool
            .call(serde_json::json!({}), &capability)
            .await
            .expect("roster remains available"),
        serde_json::json!([]),
        "rejection creates no accepted child state"
    );
    let after_rejection = budget.lock().expect("budget lock").clone();
    assert_eq!(
        (
            after_rejection.used_tokens,
            after_rejection.used_turns,
            after_rejection.used_cost,
            after_rejection.used_sub_agents,
        ),
        (0, 0, Decimal::ZERO, 0),
        "factory preparation rejection leaves immediate-parent usage unchanged"
    );

    let acknowledgement = tool
        .call(
            s060_budget_security_args("worker", 100, 10, Decimal::new(10, 0), 1, None),
            &capability,
        )
        .await
        .expect("the next spawn must reuse the parent's complete headroom");
    let child_id = s060_child_id(&acknowledgement);
    factory.wait_for_started(1).await;
    assert_eq!(factory.prepare_calls(), 2);
    assert_eq!(factory.started().len(), 1);
    assert_eq!(budget.lock().expect("budget lock").used_sub_agents, 1);

    factory.finish(&child_id, s060_budget_usage_output(0, 0, Decimal::ZERO));
    s060_join_budget_child(&sender, &root_id, &child_id).await;
    drop(roster_tool);
    drop(tool);
    drop(sender);
    actor.await.expect("supervisor actor should stop cleanly");
}

#[tokio::test]
async fn s060_spawn_journal_failure_rolls_back_reservation_and_full_headroom_is_reusable() {
    let root_id = AgentId("journal-rollback-root".into());
    let capability = s060_budget_security_capability(&["worker"]);
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        100,
        10,
        Decimal::new(10, 0),
        1,
    )));
    let factory = Arc::new(S060BudgetGateFactory::default());
    let inner_journal = Arc::new(InMemoryJournalStorage::new());
    let journal = Arc::new(S060FailFirstSpawnJournal::new(Arc::clone(&inner_journal)));
    let activity = Arc::new(S060BudgetActivityRecorder::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&budget),
        Arc::clone(&factory) as Arc<dyn TaskFactory>,
    );
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    supervisor.set_root_agent_id(root_id.clone());
    supervisor.set_activity_sink(Arc::clone(&activity) as Arc<dyn simulacra_runtime::ActivitySink>);
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    let tool = s060_budget_security_tool(&sender, &root_id.0, Arc::clone(&budget), &["worker"]);
    let roster_tool = ListChildAgentTool {
        sender: sender.clone(),
        caller_id: root_id.clone(),
    };

    let error = tool
        .call(
            s060_budget_security_args("worker", 100, 10, Decimal::new(10, 0), 1, None),
            &capability,
        )
        .await
        .expect_err("failed SubAgentSpawned append must reject spawn")
        .to_string();
    assert!(
        error.contains("journal append failed")
            && error.contains("injected first SubAgentSpawned append failure")
    );
    assert_eq!(journal.failed_spawn_appends.load(Ordering::SeqCst), 1);
    assert!(
        factory.started().is_empty(),
        "child construction must not run"
    );
    assert!(
        activity.events().is_empty(),
        "failed append emits no activity"
    );
    assert!(
        inner_journal
            .read_all(&root_id)
            .expect("root journal")
            .is_empty(),
        "failed append leaves no durable lifecycle entry"
    );
    assert_eq!(
        roster_tool
            .call(serde_json::json!({}), &capability)
            .await
            .expect("roster remains available"),
        serde_json::json!([]),
        "failed append creates no accepted child state"
    );
    let after_rejection = budget.lock().expect("budget lock").clone();
    assert_eq!(
        (
            after_rejection.used_tokens,
            after_rejection.used_turns,
            after_rejection.used_cost,
            after_rejection.used_sub_agents,
        ),
        (0, 0, Decimal::ZERO, 0),
        "journal failure rolls back every immediate-parent reservation effect"
    );

    let acknowledgement = tool
        .call(
            s060_budget_security_args("worker", 100, 10, Decimal::new(10, 0), 1, None),
            &capability,
        )
        .await
        .expect("the next spawn must reuse the parent's complete headroom");
    let child_id = s060_child_id(&acknowledgement);
    factory.wait_for_started(1).await;
    assert_eq!(factory.started().len(), 1);
    assert_eq!(budget.lock().expect("budget lock").used_sub_agents, 1);

    factory.finish(&child_id, s060_budget_usage_output(0, 0, Decimal::ZERO));
    s060_join_budget_child(&sender, &root_id, &child_id).await;
    drop(roster_tool);
    drop(tool);
    drop(sender);
    actor.await.expect("supervisor actor should stop cleanly");
}

#[tokio::test]
async fn s060_awaiting_approval_keeps_reservation_until_same_child_cancellation_settles_once() {
    let root_id = AgentId("awaiting-budget-root".into());
    let capability = s060_budget_security_capability(&["worker"]);
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        100,
        10,
        Decimal::new(10, 0),
        2,
    )));
    let factory = Arc::new(S060BudgetGateFactory::default());
    let journal = Arc::new(InMemoryJournalStorage::new());
    let activity = Arc::new(S060BudgetActivityRecorder::default());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&budget),
        Arc::clone(&factory) as Arc<dyn TaskFactory>,
    );
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    supervisor.set_root_agent_id(root_id.clone());
    supervisor.set_activity_sink(Arc::clone(&activity) as Arc<dyn simulacra_runtime::ActivitySink>);
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    let tool = s060_budget_security_tool(&sender, &root_id.0, Arc::clone(&budget), &["worker"]);
    let status_tool = ChildStatusTool {
        sender: sender.clone(),
        caller_id: root_id.clone(),
    };

    let acknowledgement = tool
        .call(
            s060_budget_security_args("worker", 100, 10, Decimal::new(10, 0), 1, None),
            &capability,
        )
        .await
        .expect("child reservation should be accepted");
    let child_id = s060_child_id(&acknowledgement);
    factory.wait_for_started(1).await;
    factory.finish(
        &child_id,
        AgentLoopOutput {
            exit_reason: ExitReason::AwaitingApproval,
            ..s060_budget_usage_output(0, 0, Decimal::ZERO)
        },
    );
    tokio::time::sleep(Duration::from_millis(25)).await;

    let running = status_tool
        .call(
            serde_json::json!({ "child_id": child_id.0 }),
            &CapabilityToken::default(),
        )
        .await
        .expect("awaiting-approval child remains queryable");
    assert_eq!(running["status"], "running");
    assert_eq!(running["ready"], false);
    let while_nonterminal = budget.lock().expect("budget lock").clone();
    assert_eq!(
        (
            while_nonterminal.used_tokens,
            while_nonterminal.used_turns,
            while_nonterminal.used_cost,
            while_nonterminal.used_sub_agents,
        ),
        (0, 0, Decimal::ZERO, 1),
        "AwaitingApproval retains reservation without converting it to actual usage"
    );
    let blocked = tool
        .call(
            s060_budget_security_args("worker", 100, 10, Decimal::new(10, 0), 1, None),
            &capability,
        )
        .await
        .expect_err("nonterminal reservation must continue consuming headroom")
        .to_string();
    assert!(
        blocked.contains("max_tokens requested 100")
            && blocked.contains("immediate parent remaining 0")
    );
    assert!(
        journal
            .read_all(&root_id)
            .expect("root journal")
            .iter()
            .all(|entry| !matches!(&entry.entry, JournalEntryKind::SubAgentCompleted { .. }))
    );
    assert!(
        activity
            .events()
            .iter()
            .all(|event| !matches!(event, simulacra_types::ActivityEvent::ChildFinished { .. }))
    );

    simulacra_runtime::CancelChildAgentTool {
        sender: sender.clone(),
        caller_id: root_id.clone(),
    }
    .call(
        serde_json::json!({ "child_id": child_id.0 }),
        &CapabilityToken::default(),
    )
    .await
    .expect("same awaiting child should acknowledge cancellation");
    tokio::time::timeout(
        Duration::from_secs(2),
        s060_join_budget_child(&sender, &root_id, &child_id),
    )
    .await
    .expect("same awaiting child must settle after cancellation");

    let after_terminal = budget.lock().expect("budget lock").clone();
    assert_eq!(
        (
            after_terminal.used_tokens,
            after_terminal.used_turns,
            after_terminal.used_cost,
            after_terminal.used_sub_agents,
        ),
        (0, 0, Decimal::ZERO, 1),
        "cancellation releases reservation, charges actual zero once, and keeps accepted count"
    );
    for _ in 0..2 {
        let terminal = status_tool
            .call(
                serde_json::json!({ "child_id": child_id.0 }),
                &CapabilityToken::default(),
            )
            .await
            .expect("cancelled terminal remains queryable");
        assert_eq!(terminal["status"]["cancelled"], serde_json::Value::Null);
    }
    assert_eq!(
        budget.lock().expect("budget lock").used_sub_agents,
        1,
        "terminal inspection cannot settle twice"
    );
    assert_eq!(
        journal
            .read_all(&root_id)
            .expect("root journal after cancellation")
            .iter()
            .filter(|entry| matches!(&entry.entry, JournalEntryKind::SubAgentCompleted { .. }))
            .count(),
        1,
        "same child has exactly one terminal journal settlement"
    );
    assert_eq!(
        activity
            .events()
            .iter()
            .filter(|event| matches!(event, simulacra_types::ActivityEvent::ChildFinished { .. }))
            .count(),
        1,
        "same child emits exactly one terminal activity event"
    );

    let reused = tool
        .call(
            s060_budget_security_args("worker", 100, 10, Decimal::new(10, 0), 1, None),
            &capability,
        )
        .await
        .expect("terminal cancellation releases the full reservation headroom");
    let reused_id = s060_child_id(&reused);
    factory.wait_for_started(2).await;
    assert_eq!(budget.lock().expect("budget lock").used_sub_agents, 2);
    factory.finish(&reused_id, s060_budget_usage_output(0, 0, Decimal::ZERO));
    s060_join_budget_child(&sender, &root_id, &reused_id).await;

    drop(status_tool);
    drop(tool);
    drop(sender);
    actor.await.expect("supervisor actor should stop cleanly");
}

#[tokio::test]
async fn s060_terminal_paths_release_reservations_charge_actual_once_and_keep_spawn_count() {
    let cases = [
        (
            "success",
            (60, 6, Decimal::new(6, 0)),
            (40, 4, Decimal::new(4, 0)),
        ),
        (
            "failure",
            (0, 0, Decimal::ZERO),
            (100, 10, Decimal::new(10, 0)),
        ),
        (
            "cancellation",
            (0, 0, Decimal::ZERO),
            (100, 10, Decimal::new(10, 0)),
        ),
        (
            "partial",
            (20, 2, Decimal::new(2, 0)),
            (80, 8, Decimal::new(8, 0)),
        ),
    ];

    for (terminal_path, actual, released_request) in cases {
        let root_id = AgentId(format!("release-{terminal_path}-root"));
        let capability = s060_budget_security_capability(&["worker"]);
        let budget = Arc::new(Mutex::new(ResourceBudget::new(
            100,
            10,
            Decimal::new(10, 0),
            0,
        )));
        let factory = Arc::new(S060BudgetGateFactory::default());
        let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
            capability.clone(),
            Arc::clone(&budget),
            Arc::clone(&factory) as Arc<dyn TaskFactory>,
        );
        supervisor.set_journal_storage(Arc::new(InMemoryJournalStorage::new()));
        supervisor.set_root_agent_id(root_id.clone());
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
        let tool = s060_budget_security_tool(&sender, &root_id.0, Arc::clone(&budget), &["worker"]);

        let first_ack = tool
            .call(
                s060_budget_security_args("worker", 60, 6, Decimal::new(6, 0), 0, None),
                &capability,
            )
            .await
            .expect("first reservation should fit");
        let first_id = s060_child_id(&first_ack);
        factory.wait_for_started(1).await;

        let while_reserved = tool
            .call(
                s060_budget_security_args("worker", 50, 5, Decimal::new(5, 0), 0, None),
                &capability,
            )
            .await
            .expect_err("outstanding reservation must prevent aggregate overcommit")
            .to_string();
        assert!(
            while_reserved.contains("max_tokens")
                && while_reserved.contains("requested 50")
                && while_reserved.contains("immediate parent")
                && while_reserved.contains("remaining 40"),
            "reservation error must expose requested and immediate-parent values: {while_reserved}"
        );
        let before_terminal = budget.lock().expect("budget lock").clone();
        assert_eq!(
            (
                before_terminal.used_tokens,
                before_terminal.used_turns,
                before_terminal.used_cost,
                before_terminal.used_sub_agents,
            ),
            (0, 0, Decimal::ZERO, 1),
            "reservation is not actual usage, while accepted child count is cumulative"
        );

        match terminal_path {
            "failure" => factory.fail(&first_id, "scripted child failure"),
            "cancellation" => {
                simulacra_runtime::CancelChildAgentTool {
                    sender: sender.clone(),
                    caller_id: root_id.clone(),
                }
                .call(
                    serde_json::json!({ "child_id": first_id.0 }),
                    &CapabilityToken::default(),
                )
                .await
                .expect("owner should be able to cancel live child");
            }
            "success" | "partial" => factory.finish(
                &first_id,
                s060_budget_usage_output(actual.0, actual.1, actual.2),
            ),
            _ => unreachable!("table only contains terminal paths"),
        }
        s060_join_budget_child(&sender, &root_id, &first_id).await;

        let after_terminal = budget.lock().expect("budget lock").clone();
        assert_eq!(
            (
                after_terminal.used_tokens,
                after_terminal.used_turns,
                after_terminal.used_cost,
                after_terminal.used_sub_agents,
            ),
            (actual.0, actual.1, actual.2, 1),
            "{terminal_path} must replace its reservation with exact actual usage exactly once"
        );
        let status_tool = ChildStatusTool {
            sender: sender.clone(),
            caller_id: root_id.clone(),
        };
        for _ in 0..2 {
            status_tool
                .call(
                    serde_json::json!({ "child_id": first_id.0 }),
                    &CapabilityToken::default(),
                )
                .await
                .expect("terminal status inspection should remain available");
        }
        let after_repeated_inspection = budget.lock().expect("budget lock").clone();
        assert_eq!(
            (
                after_repeated_inspection.used_tokens,
                after_repeated_inspection.used_turns,
                after_repeated_inspection.used_cost,
                after_repeated_inspection.used_sub_agents,
            ),
            (actual.0, actual.1, actual.2, 1),
            "terminal inspection must not charge actual usage again"
        );

        let released_ack = tool
            .call(
                s060_budget_security_args(
                    "worker",
                    released_request.0,
                    released_request.1,
                    released_request.2,
                    0,
                    None,
                ),
                &capability,
            )
            .await
            .unwrap_or_else(|error| {
                panic!("{terminal_path} must release reservation headroom: {error}")
            });
        assert_eq!(released_ack["status"], "running");
        factory.wait_for_started(2).await;
        let started = factory.started();
        assert_eq!(started.len(), 2);
        assert_eq!(started[0].budget.max_tokens, 60);
        assert_eq!(started[1].budget.max_tokens, released_request.0);
        assert_eq!(
            budget.lock().expect("budget lock").used_sub_agents,
            2,
            "used_sub_agents remains cumulative after reservation release"
        );

        factory.finish_all();
        drop(status_tool);
        drop(tool);
        drop(sender);
        actor.await.expect("supervisor actor should stop cleanly");
    }
}
