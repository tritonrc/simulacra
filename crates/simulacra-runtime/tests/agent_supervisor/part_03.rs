#[tokio::test]
async fn supervisor_spawns_agent_that_runs_to_completion() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "child-agent",
        FakeTaskPlan::Complete {
            release: None,
            output: completed_output(),
        },
    );

    let mut child_budget = default_budget();
    child_budget.used_tokens = 7;
    child_budget.used_turns = 2;
    child_budget.used_cost = Decimal::new(15, 1);

    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    let _token = supervisor
        .spawn_agent(spawn_config(
            "child-agent",
            "parent-agent",
            CapabilityToken::default(),
            child_budget.clone(),
            RestartStrategy::LetCrash,
        ))
        .expect("spawn should succeed and start the child task");

    tokio::time::timeout(
        Duration::from_secs(1),
        factory.wait_for_completion("child-agent"),
    )
    .await
    .expect("the fake child task should complete and notify the supervisor");

    assert_eq!(
        factory.completion_count("child-agent"),
        1,
        "expected the supervisor to observe exactly one successful completion"
    );
    assert_eq!(
        supervisor.parent_budget().used_tokens,
        child_budget.used_tokens,
        "expected child completion to flow back into the supervisor and roll up token usage"
    );
}

// S009 RED Assertion: the public actor loop should honor Signal > Command > Work priority.
#[tokio::test]
async fn supervisor_actor_loop_processes_messages_by_priority() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "signal-agent",
        FakeTaskPlan::Complete {
            release: None,
            output: completed_output(),
        },
    );
    factory.push_plan(
        "command-agent",
        FakeTaskPlan::Complete {
            release: None,
            output: completed_output(),
        },
    );
    factory.push_plan(
        "work-agent",
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

    let signal_tx = tx.clone();
    let command_tx = tx.clone();
    let work_tx = tx.clone();

    let (signal_result_tx, _) = tokio::sync::oneshot::channel();
    let signal_message = SupervisorMessage {
        priority: MessagePriority::Signal,
        agent_id: AgentId("signal-agent".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "signal-agent",
                "parent-agent",
                CapabilityToken::default(),
                default_budget(),
                RestartStrategy::LetCrash,
            )),
            signal_result_tx,
        ),
    };
    let (command_result_tx, _) = tokio::sync::oneshot::channel();
    let command_message = SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("command-agent".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "command-agent",
                "parent-agent",
                CapabilityToken::default(),
                default_budget(),
                RestartStrategy::LetCrash,
            )),
            command_result_tx,
        ),
    };
    let (work_result_tx, _) = tokio::sync::oneshot::channel();
    let work_message = SupervisorMessage {
        priority: MessagePriority::Work,
        agent_id: AgentId("work-agent".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "work-agent",
                "parent-agent",
                CapabilityToken::default(),
                default_budget(),
                RestartStrategy::LetCrash,
            )),
            work_result_tx,
        ),
    };

    let (signal_result, command_result, work_result) = tokio::join!(
        signal_tx.send(signal_message),
        command_tx.send(command_message),
        work_tx.send(work_message),
    );
    signal_result.expect("signal message should send");
    command_result.expect("command message should send");
    work_result.expect("work message should send");

    tokio::time::timeout(Duration::from_secs(1), factory.wait_for_started_agents(3))
        .await
        .expect("the actor loop should dispatch all queued messages");

    assert_eq!(
        factory.started_order(),
        vec![
            "signal-agent".to_string(),
            "command-agent".to_string(),
            "work-agent".to_string()
        ],
        "expected the actor loop to dispatch simultaneously queued messages in priority order"
    );

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor loop should exit once the channel closes")
        .expect("actor loop task should shut down cleanly");
}

// S009 RED Assertion: the supervisor should keep multiple child tasks alive concurrently.
#[tokio::test]
async fn supervisor_manages_multiple_concurrent_agents() {
    let factory = FakeTaskFactory::new();
    let finish_first = Arc::new(Notify::new());
    let finish_third = Arc::new(Notify::new());

    factory.push_plan(
        "agent-one",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&finish_first)),
            output: completed_output(),
        },
    );
    factory.push_plan(
        "agent-two",
        FakeTaskPlan::WaitForCancellation {
            output: completed_output(),
        },
    );
    factory.push_plan(
        "agent-three",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&finish_third)),
            output: completed_output(),
        },
    );

    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    let _token_one = supervisor
        .spawn_agent(spawn_config(
            "agent-one",
            "parent-agent",
            CapabilityToken::default(),
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect("first child should spawn");
    let token_two = supervisor
        .spawn_agent(spawn_config(
            "agent-two",
            "parent-agent",
            CapabilityToken::default(),
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect("second child should spawn");
    let _token_three = supervisor
        .spawn_agent(spawn_config(
            "agent-three",
            "parent-agent",
            CapabilityToken::default(),
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect("third child should spawn");

    tokio::time::timeout(Duration::from_secs(1), factory.wait_for_started_agents(3))
        .await
        .expect("all three children should start");

    assert_eq!(
        factory.max_running(),
        3,
        "expected the supervisor to allow all three child tasks to run concurrently"
    );

    supervisor.cancel_agent(&token_two);
    tokio::time::timeout(
        Duration::from_secs(1),
        factory.wait_for_cancellation("agent-two"),
    )
    .await
    .expect("the cancelled child should observe the cancellation signal");

    finish_first.notify_waiters();
    finish_third.notify_waiters();

    tokio::time::timeout(
        Duration::from_secs(1),
        factory.wait_for_completion("agent-one"),
    )
    .await
    .expect("the first child should continue running after a sibling is cancelled");
    tokio::time::timeout(
        Duration::from_secs(1),
        factory.wait_for_completion("agent-three"),
    )
    .await
    .expect("the third child should continue running after a sibling is cancelled");

    assert_eq!(
        factory.completion_count("agent-one"),
        1,
        "expected the first child to complete normally"
    );
    assert_eq!(
        factory.completion_count("agent-three"),
        1,
        "expected the third child to complete normally"
    );
}

// S009 RED Assertion: a failed child should be restarted by the actor loop per strategy.
#[tokio::test]
async fn supervisor_restarts_failed_agent_via_actor_loop() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "flaky-agent",
        FakeTaskPlan::Fail {
            error: RuntimeError::Session("boom".into()),
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

    let (flaky_result_tx, _) = tokio::sync::oneshot::channel();
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
            flaky_result_tx,
        ),
    })
    .await
    .expect("spawn message should send");

    tokio::time::timeout(Duration::from_secs(1), factory.wait_for_started_agents(2))
        .await
        .expect("the failed child should be restarted exactly once");

    assert_eq!(
        factory
            .started_order()
            .into_iter()
            .filter(|agent_id| agent_id == "flaky-agent")
            .count(),
        2,
        "expected retry_once to cause the actor loop to spawn the child a second time after failure"
    );

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor loop should exit once the channel closes")
        .expect("actor loop task should shut down cleanly");
}

// S009 RED Assertion: child task results should flow back to the supervisor over mpsc.
#[tokio::test]
async fn child_agents_communicate_via_mpsc() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "mpsc-child",
        FakeTaskPlan::Complete {
            release: None,
            output: completed_output(),
        },
    );

    let child_budget = default_budget();

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

    let (mpsc_result_tx, _) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("mpsc-child".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "mpsc-child",
                "parent-agent",
                CapabilityToken::default(),
                child_budget.clone(),
                RestartStrategy::LetCrash,
            )),
            mpsc_result_tx,
        ),
    })
    .await
    .expect("spawn message should send");

    tokio::time::timeout(
        Duration::from_secs(1),
        factory.wait_for_completion("mpsc-child"),
    )
    .await
    .expect("the child should complete and send its result back to the supervisor");

    // Budget rollup uses actual child AgentLoopOutput.token_usage (S018 fix),
    // not the stale SpawnConfig clone. completed_output() has 4+3=7 tokens.
    assert_eq!(
        supervisor.parent_budget().used_tokens,
        7,
        "expected the supervisor to observe the child's completion message over mpsc and roll up budget usage"
    );

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor loop should exit once the channel closes")
        .expect("actor loop task should shut down cleanly");
}
