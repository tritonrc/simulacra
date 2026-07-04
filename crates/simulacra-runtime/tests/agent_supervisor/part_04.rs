#[tokio::test]
async fn actor_join_journals_completion_before_terminal_result_resolves() {
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let parent_id = AgentId("parent-agent".into());
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "journaled-child",
        FakeTaskPlan::Complete {
            release: None,
            output: completed_output(),
        },
    );

    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory),
    );
    supervisor.set_journal_storage(Arc::clone(&journal));
    let supervisor = Arc::new(supervisor);
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let actor = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.run_actor_loop(rx).await })
    };

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("journaled-child".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "journaled-child",
                "parent-agent",
                CapabilityToken::default(),
                default_budget(),
                RestartStrategy::LetCrash,
            )),
            result_tx,
        ),
    })
    .await
    .expect("spawn message should send");

    tokio::time::timeout(Duration::from_secs(1), result_rx)
        .await
        .expect("actor should resolve the spawn ack")
        .expect("supervisor should keep the ack channel open")
        .expect("child spawn should be accepted");

    let (join_tx, join_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: parent_id.clone(),
        payload: SupervisorPayload::JoinChild(AgentId("journaled-child".into()), join_tx),
    })
    .await
    .expect("join message should send");
    tokio::time::timeout(Duration::from_secs(1), join_rx)
        .await
        .expect("actor should resolve the join result")
        .expect("supervisor should keep the join channel open")
        .expect("join should find the child")
        .result
        .expect("child should complete successfully");

    let entries = journal
        .read_all(&parent_id)
        .expect("parent journal should be readable");
    let spawned = entries
        .iter()
        .position(|entry| {
            matches!(
                &entry.entry,
                simulacra_types::JournalEntryKind::SubAgentSpawned { child_id, .. }
                if child_id.0 == "journaled-child"
            )
        })
        .expect("actor path should journal SubAgentSpawned");
    let completed = entries
        .iter()
        .position(|entry| {
            matches!(
                &entry.entry,
                simulacra_types::JournalEntryKind::SubAgentCompleted { child_id, success }
                if child_id.0 == "journaled-child" && *success
            )
        })
        .expect("actor path should journal SubAgentCompleted before join returns");
    assert!(spawned < completed);

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor loop should exit")
        .expect("actor task should shut down cleanly");
}

#[tokio::test]
async fn actor_retry_returns_successful_retry_to_original_caller() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "flaky-agent",
        FakeTaskPlan::Fail {
            error: RuntimeError::Session("first attempt failed".into()),
        },
    );
    factory.push_plan(
        "flaky-agent",
        FakeTaskPlan::Complete {
            release: None,
            output: completed_output(),
        },
    );

    let supervisor = Arc::new(AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    ));
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let actor = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.run_actor_loop(rx).await })
    };

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("flaky-agent".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "flaky-agent",
                "parent-agent",
                CapabilityToken::default(),
                default_budget(),
                RestartStrategy::RetryOnce,
            )),
            result_tx,
        ),
    })
    .await
    .expect("spawn message should send");

    let ack = tokio::time::timeout(Duration::from_secs(1), result_rx)
        .await
        .expect("actor should resolve the spawn acknowledgement")
        .expect("supervisor should keep the ack channel open")
        .expect("successful spawn should satisfy the original caller");

    assert_eq!(ack.child_id, AgentId("flaky-agent".into()));

    let (join_tx, join_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("flaky-agent".into()),
        payload: SupervisorPayload::JoinChild(AgentId("flaky-agent".into()), join_tx),
    })
    .await
    .expect("join message should send");

    let terminal = tokio::time::timeout(Duration::from_secs(1), join_rx)
        .await
        .expect("actor should resolve the joined child result")
        .expect("supervisor should keep the join channel open")
        .expect("successful retry should satisfy the join caller");

    assert_eq!(terminal.result.unwrap().exit_reason, ExitReason::Complete);
    assert_eq!(
        factory
            .started_order()
            .into_iter()
            .filter(|agent_id| agent_id == "flaky-agent")
            .count(),
        2
    );

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor loop should exit")
        .expect("actor task should shut down cleanly");
}

#[test]
fn valid_spawn_without_task_factory_has_no_spawn_side_effects() {
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let parent_id = AgentId("parent-agent".into());
    let mut supervisor = AgentSupervisor::new(CapabilityToken::default(), default_budget());
    supervisor.set_journal_storage(Arc::clone(&journal));

    let result = supervisor.spawn_agent(spawn_config(
        "missing-factory-child",
        "parent-agent",
        CapabilityToken::default(),
        default_budget(),
        RestartStrategy::LetCrash,
    ));

    assert!(matches!(result, Err(RuntimeError::SpawnMissingTask)));
    assert_eq!(supervisor.parent_budget().used_sub_agents, 0);
    assert!(
        journal
            .read_all(&parent_id)
            .expect("parent journal should be readable")
            .is_empty()
    );
}
