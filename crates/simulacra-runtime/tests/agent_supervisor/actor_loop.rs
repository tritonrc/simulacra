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

    drop(signal_tx);
    drop(command_tx);
    drop(work_tx);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor loop should exit once the channel closes")
        .expect("actor loop task should shut down cleanly");
}

#[test]
fn s054_cancel_priority_wins_over_status_wait_and_close_commands() {
    let mut queue = std::collections::BinaryHeap::new();

    let (status_tx, _) = tokio::sync::oneshot::channel();
    queue.push(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::ChildStatus(AgentId("child-1".into()), status_tx),
    });

    let (wait_tx, _) = tokio::sync::oneshot::channel();
    queue.push(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChild(
            AgentId("child-1".into()),
            Duration::from_millis(1),
            wait_tx,
        ),
    });

    let (wait_any_tx, _) = tokio::sync::oneshot::channel();
    queue.push(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChildren(
            vec![AgentId("child-1".into()), AgentId("child-2".into())],
            Duration::from_millis(1),
            wait_any_tx,
        ),
    });

    let (close_tx, _) = tokio::sync::oneshot::channel();
    queue.push(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::CloseChild(AgentId("child-1".into()), close_tx),
    });

    let (cancel_tx, _) = tokio::sync::oneshot::channel();
    queue.push(SupervisorMessage {
        priority: MessagePriority::Signal,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::CancelChild(AgentId("child-1".into()), cancel_tx),
    });

    let first = queue
        .pop()
        .expect("queued child-control messages should pop by priority");
    assert!(
        matches!(first.payload, SupervisorPayload::CancelChild(_, _)),
        "signal-priority cancel must be processed before S054 command-priority probes"
    );
    assert!(
        queue
            .into_iter()
            .all(|message| matches!(
                message.payload,
                SupervisorPayload::ChildStatus(_, _)
                    | SupervisorPayload::WaitChild(_, _, _)
                    | SupervisorPayload::WaitChildren(_, _, _)
                    | SupervisorPayload::CloseChild(_, _)
            )),
        "remaining S054 messages should all be command-priority probes"
    );
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

#[tokio::test]
async fn cancel_child_payload_signals_only_the_target_child_token() {
    #[derive(Clone)]
    struct TokenRecordingFactory {
        tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
        notify: Arc<Notify>,
    }

    impl TaskFactory for TokenRecordingFactory {
        fn create_task(&self, config: SpawnConfig, token: CancellationToken) -> BoxTaskFuture {
            self.tokens
                .lock()
                .unwrap()
                .insert(config.agent_id.0.clone(), token.clone());
            self.notify.notify_waiters();
            Box::pin(async move {
                while !token.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Ok(AgentLoopOutput {
                    exit_reason: ExitReason::Cancelled,
                    messages: vec![],
                    token_usage: TokenUsage::default(),
                    used_turns: 0,
                    used_cost: Decimal::ZERO,
                })
            })
        }
    }

    let tokens = Arc::new(Mutex::new(HashMap::new()));
    let notify = Arc::new(Notify::new());
    let factory = TokenRecordingFactory {
        tokens: Arc::clone(&tokens),
        notify: Arc::clone(&notify),
    };
    let supervisor = Arc::new(AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory),
    ));
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let actor = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.run_actor_loop(rx).await })
    };

    for child_id in ["child-a", "child-b"] {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tx.send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId(child_id.into()),
            payload: SupervisorPayload::Spawn(
                Box::new(spawn_config(
                    child_id,
                    "parent-agent",
                    CapabilityToken::default(),
                    default_budget(),
                    RestartStrategy::LetCrash,
                )),
                ack_tx,
            ),
        })
        .await
        .expect("spawn message should send");
        ack_rx
            .await
            .expect("spawn ack channel should stay open")
            .expect("spawn should be accepted");
    }

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tokens.lock().unwrap().len() == 2 {
                break;
            }
            notify.notified().await;
        }
    })
    .await
    .expect("both child tokens should be recorded");

    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Signal,
        agent_id: AgentId("child-b".into()),
        payload: SupervisorPayload::CancelChild(AgentId("child-b".into()), cancel_tx),
    })
    .await
    .expect("cancel message should send");
    cancel_rx
        .await
        .expect("cancel response channel should stay open")
        .expect("targeted cancel should succeed");

    {
        let snapshot = tokens.lock().unwrap();
        assert!(
            !snapshot.get("child-a").unwrap().is_cancelled(),
            "cancel_child_agent must not cancel sibling children"
        );
        assert!(
            snapshot.get("child-b").unwrap().is_cancelled(),
            "cancel_child_agent must signal the targeted child"
        );
    }

    let (join_b_tx, join_b_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::JoinChild(AgentId("child-b".into()), join_b_tx),
    })
    .await
    .expect("join message should send");
    tokio::time::timeout(Duration::from_secs(1), join_b_rx)
        .await
        .expect("join should finish after cancellation")
        .expect("join response channel should stay open")
        .expect("join should return the cancelled child result");

    let (cancel_completed_tx, cancel_completed_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Signal,
        agent_id: AgentId("child-b".into()),
        payload: SupervisorPayload::CancelChild(AgentId("child-b".into()), cancel_completed_tx),
    })
    .await
    .expect("cancel-completed message should send");
    let cancel_completed = cancel_completed_rx
        .await
        .expect("cancel-completed response channel should stay open");
    assert!(
        matches!(cancel_completed, Err(ref msg) if msg.contains("already completed")),
        "completed children should no longer accept cancellation: {cancel_completed:?}"
    );

    let (cancel_a_tx, cancel_a_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Signal,
        agent_id: AgentId("child-a".into()),
        payload: SupervisorPayload::CancelChild(AgentId("child-a".into()), cancel_a_tx),
    })
    .await
    .expect("second cancel message should send");
    cancel_a_rx
        .await
        .expect("second cancel response channel should stay open")
        .expect("second targeted cancel should succeed");

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor loop should exit after cancelled children finish and channel closes")
        .expect("actor loop task should shut down cleanly");
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

#[tokio::test]
async fn steering_unknown_child_returns_error() {
    let supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(FakeTaskFactory::new()),
    );

    let result = supervisor.steer_child(&AgentId("missing-child".into()), "focus".into());
    assert!(
        matches!(result, Err(ref message) if message.contains("unknown child_id")),
        "unknown child steering should fail: {result:?}"
    );
}

#[tokio::test]
async fn steering_live_child_is_accepted_and_completed_child_is_rejected() {
    let release = Arc::new(Notify::new());
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "live-child",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&release)),
            output: AgentLoopOutput {
                exit_reason: ExitReason::Complete,
                messages: vec![],
                token_usage: TokenUsage::default(),
                used_turns: 0,
                used_cost: Decimal::ZERO,
            },
        },
    );
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    supervisor
        .spawn_agent(spawn_config(
            "live-child",
            "parent-agent",
            CapabilityToken::default(),
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect("live child spawn should be accepted");
    factory.wait_for_started_agents(1).await;

    supervisor
        .steer_child(&AgentId("live-child".into()), "add detail".into())
        .expect("live child steering should enqueue");

    release.notify_waiters();
    factory.wait_for_completion("live-child").await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    let completed = supervisor.steer_child(&AgentId("live-child".into()), "too late".into());
    assert!(
        matches!(completed, Err(ref message) if message.contains("already completed")),
        "completed child steering should fail: {completed:?}"
    );
}
