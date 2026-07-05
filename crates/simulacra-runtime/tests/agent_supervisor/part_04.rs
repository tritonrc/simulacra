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

#[tokio::test]
async fn child_status_wait_and_close_follow_handle_lifecycle() {
    let factory = FakeTaskFactory::new();
    let release = Arc::new(Notify::new());
    factory.push_plan(
        "child-orchestrated",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&release)),
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

    let mut config = spawn_config(
        "child-orchestrated",
        "parent-agent",
        CapabilityToken::default(),
        default_budget(),
        RestartStrategy::LetCrash,
    );
    config.agent_type = Some("researcher".into());
    config.task = "inspect lifecycle".into();

    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::Spawn(Box::new(config), ack_tx),
    })
    .await
    .expect("spawn message should send");
    ack_rx
        .await
        .expect("spawn ack channel should stay open")
        .expect("spawn should be accepted");
    tokio::time::timeout(Duration::from_secs(1), factory.wait_for_started_agents(1))
        .await
        .expect("child should start");

    let (status_tx, status_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::ChildStatus(AgentId("child-orchestrated".into()), status_tx),
    })
    .await
    .expect("status message should send");
    let status = status_rx
        .await
        .expect("status response channel should stay open")
        .expect("running child should have status");
    assert_eq!(status.child_id.0, "child-orchestrated");
    assert_eq!(status.agent_type, "researcher");
    assert_eq!(status.status, "running");
    assert!(!status.ready);

    let (poll_tx, poll_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChild(
            AgentId("child-orchestrated".into()),
            Duration::ZERO,
            poll_tx,
        ),
    })
    .await
    .expect("poll message should send");
    let poll = poll_rx
        .await
        .expect("poll response channel should stay open")
        .expect("poll should succeed");
    assert_eq!(poll.status, "running");
    assert!(!poll.ready);

    let (timeout_tx, timeout_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChild(
            AgentId("child-orchestrated".into()),
            Duration::from_millis(10),
            timeout_tx,
        ),
    })
    .await
    .expect("bounded wait message should send");
    let timeout = tokio::time::timeout(Duration::from_secs(1), timeout_rx)
        .await
        .expect("bounded wait should time out promptly")
        .expect("timeout response channel should stay open")
        .expect("timeout should be a non-error running result");
    assert_eq!(timeout.status, "running");
    assert!(!timeout.ready);

    let (close_running_tx, close_running_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::CloseChild(
            AgentId("child-orchestrated".into()),
            close_running_tx,
        ),
    })
    .await
    .expect("close-running message should send");
    let close_running = close_running_rx
        .await
        .expect("close-running response channel should stay open");
    assert!(
        matches!(close_running, Err(ref message) if message.contains("still running")),
        "close must reject running children: {close_running:?}"
    );

    let (terminal_wait_tx, terminal_wait_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChild(
            AgentId("child-orchestrated".into()),
            Duration::from_secs(1),
            terminal_wait_tx,
        ),
    })
    .await
    .expect("terminal wait message should send");
    release.notify_waiters();
    let terminal_wait = terminal_wait_rx
        .await
        .expect("terminal wait response channel should stay open")
        .expect("terminal wait should succeed");
    assert_eq!(terminal_wait.status, "completed");
    assert!(terminal_wait.ready);
    assert_eq!(
        terminal_wait
            .terminal
            .as_ref()
            .expect("terminal result should be retained")
            .agent_type,
        "researcher"
    );

    let (terminal_poll_tx, terminal_poll_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChild(
            AgentId("child-orchestrated".into()),
            Duration::ZERO,
            terminal_poll_tx,
        ),
    })
    .await
    .expect("terminal poll message should send");
    let terminal_poll = terminal_poll_rx
        .await
        .expect("terminal poll response channel should stay open")
        .expect("terminal poll should return the retained result immediately");
    assert_eq!(terminal_poll.status, "completed");
    assert!(terminal_poll.ready);
    assert!(
        terminal_poll.terminal.is_some(),
        "zero-timeout wait should return terminal data when the child is already complete"
    );

    let (join_tx, join_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::JoinChild(AgentId("child-orchestrated".into()), join_tx),
    })
    .await
    .expect("join after wait message should send");
    let joined = join_rx
        .await
        .expect("join response channel should stay open")
        .expect("join should still see non-consumed terminal result");
    assert_eq!(joined.result.unwrap().exit_reason, ExitReason::Complete);

    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::CloseChild(AgentId("child-orchestrated".into()), close_tx),
    })
    .await
    .expect("close message should send");
    close_rx
        .await
        .expect("close response channel should stay open")
        .expect("close should release terminal child state");

    let (unknown_close_tx, unknown_close_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::CloseChild(
            AgentId("child-orchestrated".into()),
            unknown_close_tx,
        ),
    })
    .await
    .expect("unknown-close message should send");
    let unknown_close = unknown_close_rx
        .await
        .expect("unknown-close response channel should stay open");
    assert!(
        matches!(unknown_close, Err(ref message) if message.contains("unknown or closed")),
        "close should fail after terminal child state has been released: {unknown_close:?}"
    );

    let (closed_status_tx, closed_status_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::ChildStatus(
            AgentId("child-orchestrated".into()),
            closed_status_tx,
        ),
    })
    .await
    .expect("closed status message should send");
    let closed_status = closed_status_rx
        .await
        .expect("closed status response channel should stay open");
    assert!(
        matches!(closed_status, Err(ref message) if message.contains("unknown or closed")),
        "closed children should be unknown to status: {closed_status:?}"
    );

    let (closed_join_tx, closed_join_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::JoinChild(AgentId("child-orchestrated".into()), closed_join_tx),
    })
    .await
    .expect("closed join message should send");
    assert!(
        closed_join_rx
            .await
            .expect("closed join response channel should stay open")
            .is_err(),
        "join should fail after close"
    );

    let (closed_wait_tx, closed_wait_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChild(
            AgentId("child-orchestrated".into()),
            Duration::ZERO,
            closed_wait_tx,
        ),
    })
    .await
    .expect("closed wait message should send");
    assert!(
        closed_wait_rx
            .await
            .expect("closed wait response channel should stay open")
            .is_err(),
        "wait should fail after close"
    );

    let (closed_steer_tx, closed_steer_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::SteerChild(
            AgentId("child-orchestrated".into()),
            "more input".into(),
            closed_steer_tx,
        ),
    })
    .await
    .expect("closed steer message should send");
    assert!(
        closed_steer_rx
            .await
            .expect("closed steer response channel should stay open")
            .is_err(),
        "steer should fail after close"
    );

    let (closed_cancel_tx, closed_cancel_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Signal,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::CancelChild(
            AgentId("child-orchestrated".into()),
            closed_cancel_tx,
        ),
    })
    .await
    .expect("closed cancel message should send");
    assert!(
        closed_cancel_rx
            .await
            .expect("closed cancel response channel should stay open")
            .is_err(),
        "cancel should fail after close"
    );

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor should exit after channel closes")
        .expect("actor task should shut down cleanly");
}

#[tokio::test]
async fn child_status_reports_failed_and_cancelled_terminal_states() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "child-failed",
        FakeTaskPlan::Fail {
            error: RuntimeError::Session("boom".into()),
        },
    );
    factory.push_plan(
        "child-cancelled",
        FakeTaskPlan::WaitForCancellation {
            output: AgentLoopOutput {
                exit_reason: ExitReason::Cancelled,
                messages: vec![],
                token_usage: TokenUsage::default(),
                used_turns: 0,
                used_cost: Decimal::ZERO,
            },
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

    for child_id in ["child-failed", "child-cancelled"] {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tx.send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-agent".into()),
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

    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Signal,
        agent_id: AgentId("child-cancelled".into()),
        payload: SupervisorPayload::CancelChild(AgentId("child-cancelled".into()), cancel_tx),
    })
    .await
    .expect("cancel message should send");
    cancel_rx
        .await
        .expect("cancel response channel should stay open")
        .expect("cancel should succeed");

    for (child_id, expected_status) in [
        ("child-failed", "failed"),
        ("child-cancelled", "cancelled"),
    ] {
        let (wait_tx, wait_rx) = tokio::sync::oneshot::channel();
        tx.send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-agent".into()),
            payload: SupervisorPayload::WaitChild(
                AgentId(child_id.into()),
                Duration::from_secs(1),
                wait_tx,
            ),
        })
        .await
        .expect("wait message should send");
        let wait = tokio::time::timeout(Duration::from_secs(1), wait_rx)
            .await
            .expect("terminal wait should resolve")
            .expect("wait response channel should stay open")
            .expect("terminal wait should succeed");
        assert_eq!(wait.status, expected_status);
        assert!(wait.ready);
    }

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor should exit after channel closes")
        .expect("actor task should shut down cleanly");
}

#[tokio::test]
async fn join_child_terminal_result_includes_elapsed_ms_and_structured_tool_use_count() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "child-with-tools",
        FakeTaskPlan::Complete {
            release: None,
            output: AgentLoopOutput {
                exit_reason: ExitReason::Complete,
                messages: vec![
                    Message {
                        role: Role::Tool,
                        content: "tool one".into(),
                        tool_calls: vec![],
                        tool_call_id: Some("tool-1".into()),
                    },
                    Message {
                        role: Role::Assistant,
                        content: "middle".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                    },
                    Message {
                        role: Role::Tool,
                        content: "tool two".into(),
                        tool_calls: vec![],
                        tool_call_id: Some("tool-2".into()),
                    },
                    Message {
                        role: Role::Assistant,
                        content: "done".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                    },
                ],
                token_usage: TokenUsage {
                    input_tokens: 3,
                    output_tokens: 2,
                },
                used_turns: 1,
                used_cost: Decimal::ZERO,
            },
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

    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "child-with-tools",
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

    let (join_tx, join_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::JoinChild(AgentId("child-with-tools".into()), join_tx),
    })
    .await
    .expect("join message should send");
    let terminal = join_rx
        .await
        .expect("join response channel should stay open")
        .expect("join should return terminal metadata");
    assert_eq!(terminal.tool_uses, 2);
    assert!(
        terminal.elapsed_ms < 60_000,
        "elapsed_ms should be populated from supervisor timing, got {}",
        terminal.elapsed_ms
    );

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor should exit after channel closes")
        .expect("actor task should shut down cleanly");
}

#[tokio::test]
async fn wait_children_returns_running_on_timeout_and_first_terminal_child() {
    let release_a = Arc::new(Notify::new());
    let release_b = Arc::new(Notify::new());
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "child-a",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&release_a)),
            output: wait_any_output("child a done", 2, 3),
        },
    );
    factory.push_plan(
        "child-b",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&release_b)),
            output: wait_any_output("child b done", 7, 11),
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

    for child_id in ["child-a", "child-b"] {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tx.send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-agent".into()),
            payload: SupervisorPayload::Spawn(
                Box::new(spawn_config(
                    child_id,
                    "parent-agent",
                    CapabilityToken::default(),
                    wait_any_child_budget(),
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
    tokio::time::timeout(Duration::from_secs(1), factory.wait_for_started_agents(2))
        .await
        .expect("both children should start");

    let (unknown_tx, unknown_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChildren(
            vec![AgentId("child-a".into()), AgentId("missing-child".into())],
            Duration::ZERO,
            unknown_tx,
        ),
    })
    .await
    .expect("unknown-child wait-any message should send");
    let unknown = unknown_rx
        .await
        .expect("unknown-child response channel should stay open");
    assert!(
        matches!(unknown, Err(ref message) if message.contains("unknown or closed")),
        "wait-any should reject unknown children before registering waiters: {unknown:?}"
    );

    let (poll_tx, poll_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChildren(
            vec![AgentId("child-a".into()), AgentId("child-b".into())],
            Duration::ZERO,
            poll_tx,
        ),
    })
    .await
    .expect("zero-timeout wait-any message should send");
    let poll = poll_rx
        .await
        .expect("zero-timeout response channel should stay open")
        .expect("zero-timeout running wait-any should succeed");
    assert_eq!(poll.status, "running");
    assert!(!poll.ready);
    assert!(poll.terminal.is_none());

    let (timeout_tx, timeout_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChildren(
            vec![AgentId("child-a".into()), AgentId("child-b".into())],
            Duration::from_millis(10),
            timeout_tx,
        ),
    })
    .await
    .expect("wait-any timeout message should send");
    let timeout = tokio::time::timeout(Duration::from_secs(1), timeout_rx)
        .await
        .expect("wait-any should honor bounded timeout")
        .expect("timeout response channel should stay open")
        .expect("timeout should be a non-error running result");
    assert_eq!(timeout.status, "running");
    assert!(!timeout.ready);
    assert_eq!(
        timeout
            .child_ids
            .iter()
            .map(|child_id| child_id.0.as_str())
            .collect::<Vec<_>>(),
        vec!["child-a", "child-b"]
    );

    let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChildren(
            vec![AgentId("child-a".into()), AgentId("child-b".into())],
            Duration::from_secs(1),
            terminal_tx,
        ),
    })
    .await
    .expect("wait-any terminal message should send");
    release_b.notify_waiters();
    let terminal = terminal_rx
        .await
        .expect("terminal response channel should stay open")
        .expect("terminal wait-any should succeed");
    assert_eq!(terminal.status, "completed");
    assert!(terminal.ready);
    let terminal_result = terminal
        .terminal
        .as_ref()
        .expect("terminal wait-any should include the completed child result");
    assert_eq!(terminal_result.child_id.0, "child-b");
    let output = terminal_result
        .result
        .as_ref()
        .expect("child-b should complete successfully");
    assert_eq!(output.messages.last().map(|message| message.content.as_str()), Some("child b done"));
    assert_eq!(output.token_usage.input_tokens, 7);
    assert_eq!(output.token_usage.output_tokens, 11);

    let (join_tx, join_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::JoinChild(AgentId("child-b".into()), join_tx),
    })
    .await
    .expect("join after wait-any message should send");
    join_rx
        .await
        .expect("join response channel should stay open")
        .expect("join should still see wait-any terminal result");

    let (repeat_tx, repeat_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChildren(
            vec![AgentId("child-a".into()), AgentId("child-b".into())],
            Duration::ZERO,
            repeat_tx,
        ),
    })
    .await
    .expect("repeat wait-any message should send");
    let repeat = repeat_rx
        .await
        .expect("repeat response channel should stay open")
        .expect("repeat wait-any should still see terminal result");
    assert_eq!(
        repeat
            .terminal
            .as_ref()
            .expect("repeat wait-any should include terminal result")
            .child_id
            .0,
        "child-b"
    );

    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::CloseChild(AgentId("child-b".into()), close_tx),
    })
    .await
    .expect("close after wait-any message should send");
    close_rx
        .await
        .expect("close response channel should stay open")
        .expect("close should release wait-any terminal child");

    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChildren(
            vec![AgentId("child-b".into())],
            Duration::ZERO,
            closed_tx,
        ),
    })
    .await
    .expect("closed-child wait-any message should send");
    let closed = closed_rx
        .await
        .expect("closed-child response channel should stay open");
    assert!(
        matches!(closed, Err(ref message) if message.contains("unknown or closed")),
        "wait-any should fail after a terminal child is closed: {closed:?}"
    );

    release_a.notify_waiters();
    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor should exit after channel closes")
        .expect("actor task should shut down cleanly");
}

#[tokio::test]
async fn wait_children_zero_timeout_returns_first_already_terminal_child_in_input_order() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "child-a",
        FakeTaskPlan::Complete {
            release: None,
            output: wait_any_output("child a done", 2, 3),
        },
    );
    factory.push_plan(
        "child-b",
        FakeTaskPlan::Complete {
            release: None,
            output: wait_any_output("child b done", 7, 11),
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

    for child_id in ["child-a", "child-b"] {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tx.send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-agent".into()),
            payload: SupervisorPayload::Spawn(
                Box::new(spawn_config(
                    child_id,
                    "parent-agent",
                    CapabilityToken::default(),
                    wait_any_child_budget(),
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
    tokio::time::timeout(Duration::from_secs(1), factory.wait_for_completion("child-a"))
        .await
        .expect("child-a should complete");
    tokio::time::timeout(Duration::from_secs(1), factory.wait_for_completion("child-b"))
        .await
        .expect("child-b should complete");

    let (wait_tx, wait_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChildren(
            vec![AgentId("child-b".into()), AgentId("child-a".into())],
            Duration::ZERO,
            wait_tx,
        ),
    })
    .await
    .expect("zero-timeout wait-any message should send");
    let wait = wait_rx
        .await
        .expect("wait-any response channel should stay open")
        .expect("already-terminal wait-any should succeed");
    assert_eq!(
        wait.terminal
            .as_ref()
            .expect("terminal result should be present")
            .child_id
            .0,
        "child-b",
        "when multiple children are already terminal, input order chooses the result"
    );

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor should exit after channel closes")
        .expect("actor task should shut down cleanly");
}

fn wait_any_output(message: &str, input_tokens: u64, output_tokens: u64) -> AgentLoopOutput {
    AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![Message {
            role: Role::Assistant,
            content: message.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        token_usage: TokenUsage {
            input_tokens,
            output_tokens,
        },
        used_turns: 1,
        used_cost: Decimal::ZERO,
    }
}

fn wait_any_child_budget() -> ResourceBudget {
    ResourceBudget::new(1_000, 1, Decimal::new(100, 0), 1)
}
