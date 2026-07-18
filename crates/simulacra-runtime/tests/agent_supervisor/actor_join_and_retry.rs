async fn request_child_roster(
    tx: &tokio::sync::mpsc::Sender<SupervisorMessage>,
) -> Vec<simulacra_runtime::ChildRosterEntry> {
    let (list_tx, list_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::ListChildren(list_tx),
    })
    .await
    .expect("list children message should send");
    list_rx
        .await
        .expect("list children response channel should stay open")
        .expect("list children should succeed")
}

#[test]
fn child_agent_status_failed_without_content_uses_an_explicit_null_wire_value() {
    assert_eq!(
        serde_json::to_value(ChildAgentStatus::Failed(None))
            .expect("child status should serialize"),
        serde_json::json!({ "failed": null })
    );
}

#[cfg(feature = "spawn")]
async fn call_child_status_json(
    tx: &tokio::sync::mpsc::Sender<SupervisorMessage>,
    child_id: &str,
) -> Result<serde_json::Value, ToolError> {
    ChildStatusTool { sender: tx.clone() }
        .call(
            serde_json::json!({ "child_id": child_id }),
            &CapabilityToken::default(),
        )
        .await
}

#[cfg(feature = "spawn")]
async fn call_child_roster_json(
    tx: &tokio::sync::mpsc::Sender<SupervisorMessage>,
) -> Result<serde_json::Value, ToolError> {
    ListChildAgentTool { sender: tx.clone() }
        .call(serde_json::json!({}), &CapabilityToken::default())
        .await
}

#[cfg(feature = "spawn")]
fn output_with_assistant(content: Option<&str>, exit_reason: ExitReason) -> AgentLoopOutput {
    AgentLoopOutput {
        exit_reason,
        messages: content
            .map(|content| Message {
                role: Role::Assistant,
                content: content.into(),
                tool_calls: vec![],
                tool_call_id: None,
                provider_content: vec![],
            })
            .into_iter()
            .collect(),
        token_usage: TokenUsage::default(),
        reported_tool_uses: None,
        used_turns: 1,
        used_cost: Decimal::ZERO,
    }
}

#[tokio::test]
#[cfg(feature = "spawn")]
async fn status_and_roster_expose_cached_terminal_content_without_consuming_it() {
    let factory = FakeTaskFactory::new();
    let running_release = Arc::new(Notify::new());
    factory.push_plan(
        "z-running",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&running_release)),
            output: output_with_assistant(Some("later"), ExitReason::Complete),
        },
    );
    factory.push_plan(
        "a-completed",
        FakeTaskPlan::Complete {
            release: None,
            output: output_with_assistant(Some("real finding"), ExitReason::Complete),
        },
    );
    factory.push_plan(
        "b-null",
        FakeTaskPlan::Complete {
            release: None,
            output: output_with_assistant(None, ExitReason::Complete),
        },
    );
    factory.push_plan(
        "c-empty",
        FakeTaskPlan::Complete {
            release: None,
            output: output_with_assistant(Some(""), ExitReason::Complete),
        },
    );
    factory.push_plan(
        "d-failed",
        FakeTaskPlan::Fail {
            error: RuntimeError::Session("supervisor boom".into()),
        },
    );
    factory.push_plan(
        "e-cancelled",
        FakeTaskPlan::Complete {
            release: None,
            output: output_with_assistant(Some("cancel reason"), ExitReason::Cancelled),
        },
    );
    factory.push_plan(
        "f-cancelled-null",
        FakeTaskPlan::Complete {
            release: None,
            output: output_with_assistant(None, ExitReason::Cancelled),
        },
    );
    factory.push_plan(
        "g-partial",
        FakeTaskPlan::Complete {
            release: None,
            output: AgentLoopOutput {
                exit_reason: ExitReason::BudgetExhausted,
                messages: vec![
                    Message { role: Role::Assistant, content: "partial finding".into(), tool_calls: vec![], tool_call_id: None, provider_content: vec![] },
                    Message { role: Role::Tool, content: "late tool result".into(), tool_calls: vec![], tool_call_id: Some("tool-call-1".into()), provider_content: vec![] },
                ],
                token_usage: TokenUsage::default(),
                reported_tool_uses: None,
                used_turns: 1,
                used_cost: Decimal::ZERO,
            },
        },
    );
    factory.push_plan(
        "h-error-output",
        FakeTaskPlan::Complete {
            release: None,
            output: output_with_assistant(
                Some("partial answer before provider failure"),
                ExitReason::Error("provider timeout".into()),
            ),
        },
    );

    let supervisor = Arc::new(AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        ResourceBudget::new(1_000_000, 100, Decimal::new(1_000, 0), 10),
        Arc::new(factory.clone()),
    ));
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let actor = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.run_actor_loop(rx).await })
    };

    for child_id in [
        "z-running",
        "a-completed",
        "b-null",
        "c-empty",
        "d-failed",
        "e-cancelled",
        "f-cancelled-null",
        "g-partial",
        "h-error-output",
    ] {
        let mut config = spawn_config(
            child_id,
            "parent-agent",
            CapabilityToken::default(),
            ResourceBudget::new(100, 1, Decimal::ZERO, 0),
            RestartStrategy::LetCrash,
        );
        config.agent_type = Some("researcher".into());
        config.task = format!("task for {child_id}");
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
            .expect("spawn response channel should stay open")
            .expect("spawn should be accepted");
    }
    tokio::time::timeout(Duration::from_secs(1), factory.wait_for_started_agents(9))
        .await
        .expect("all child tasks should start");
    for child_id in [
        "a-completed",
        "b-null",
        "c-empty",
        "d-failed",
        "e-cancelled",
        "f-cancelled-null",
        "g-partial",
        "h-error-output",
    ] {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if call_child_status_json(&tx, child_id)
                    .await
                    .expect("terminal status probe should succeed")["ready"]
                    == true
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
            .await
            .expect("terminal child should finish");
    }

    assert_eq!(
        call_child_status_json(&tx, "z-running")
            .await
            .expect("running status should succeed")["status"],
        serde_json::json!("running")
    );

    let expected_terminal_statuses = [
        ("g-partial", serde_json::json!({ "completed": "partial finding" })),
        ("a-completed", serde_json::json!({ "completed": "real finding" })),
        ("b-null", serde_json::json!({ "completed": null })),
        ("c-empty", serde_json::json!({ "completed": "" })),
        ("d-failed", serde_json::json!({ "failed": "session error: supervisor boom" })),
        ("e-cancelled", serde_json::json!({ "cancelled": "cancel reason" })),
        ("f-cancelled-null", serde_json::json!({ "cancelled": null })),
        ("h-error-output", serde_json::json!({ "failed": "provider timeout" })),
    ];
    for (child_id, expected_status) in &expected_terminal_statuses {
        let first = call_child_status_json(&tx, child_id)
            .await
            .expect("first status peek should succeed");
        let second = call_child_status_json(&tx, child_id)
            .await
            .expect("repeat status peek should succeed");
        assert_eq!(first["status"], *expected_status, "wrong status for {child_id}");
        assert_eq!(second, first, "status peeks must clone cached content");
        assert_eq!(first["child_id"], *child_id);
        assert_eq!(first["agent_type"], "researcher");
        assert_eq!(first["ready"], true);
        assert!(first["elapsed_ms"].is_u64(), "elapsed_ms must remain present");
    }

    let first_roster = call_child_roster_json(&tx)
        .await
        .expect("first roster peek should succeed");
    let second_roster = call_child_roster_json(&tx)
        .await
        .expect("repeat roster peek should succeed");
    let roster = first_roster.as_array().expect("roster should be an array");
    let repeated_roster = second_roster.as_array().expect("repeat roster should be an array");
    let ordered_ids = roster
        .iter()
        .map(|entry| entry["child_id"].as_str().expect("child id"))
        .collect::<Vec<_>>();
    assert_eq!(
        ordered_ids,
        vec![
            "a-completed",
            "b-null",
            "c-empty",
            "d-failed",
            "e-cancelled",
            "f-cancelled-null",
            "g-partial",
            "h-error-output",
            "z-running",
        ]
    );
    for (index, entry) in roster.iter().enumerate() {
        assert_eq!(entry["agent_type"], "researcher");
        assert_eq!(entry["task"], format!("task for {}", ordered_ids[index]));
        assert!(entry["elapsed_ms"].is_u64(), "elapsed_ms must remain present");
        assert_eq!(repeated_roster[index]["child_id"], entry["child_id"]);
        assert_eq!(repeated_roster[index]["status"], entry["status"]);
    }
    assert_eq!(roster[0]["ready"], true);
    assert_eq!(roster[0]["status"], serde_json::json!({ "completed": "real finding" }));
    assert_eq!(roster[5]["status"], serde_json::json!({ "cancelled": null }));
    assert_eq!(roster[6]["status"], serde_json::json!({ "completed": "partial finding" }));
    assert_eq!(roster[7]["status"], serde_json::json!({ "failed": "provider timeout" }));
    assert_eq!(roster[8]["status"], "running");
    assert_eq!(roster[8]["ready"], false);

    let join_tool = JoinChildAgentTool { sender: tx.clone() };
    let joined = join_tool
        .call(
            serde_json::json!({ "child_id": "a-completed" }),
            &CapabilityToken::default(),
        )
        .await
        .expect("join after peeks should still succeed");
    let joined_again = join_tool
        .call(
            serde_json::json!({ "child_id": "a-completed" }),
            &CapabilityToken::default(),
        )
        .await
        .expect("repeat join should clone the same cached terminal result");
    assert_eq!(joined_again, joined, "status/list peeks and joins must not consume or alter the summary");
    assert_eq!(joined["child_id"], "a-completed");
    assert_eq!(joined["agent_type"], "researcher");
    assert_eq!(joined["status"], "completed");
    assert_eq!(joined["ready"], true);
    assert_eq!(joined["exit_reason"], "completed");
    assert_eq!(joined["message"], "real finding");
    assert!(joined["elapsed_ms"].is_u64());
    assert!(joined["tool_uses"].is_u64());
    assert!(joined["token_usage"].is_object());
    assert_eq!(joined["artifacts"], serde_json::json!([]));
    assert_eq!(joined["vfs_changes"], serde_json::json!([]));
    assert_eq!(
        call_child_status_json(&tx, "a-completed")
            .await
            .expect("status after join should retain content")["status"],
        serde_json::json!({ "completed": "real finding" })
    );

    let partial_join = join_tool
        .call(
            serde_json::json!({ "child_id": "g-partial" }),
            &CapabilityToken::default(),
        )
        .await
        .expect("partial child join should succeed");
    assert_eq!(partial_join["message"], "partial finding");

    let error_output_join = join_tool
        .call(
            serde_json::json!({ "child_id": "h-error-output" }),
            &CapabilityToken::default(),
        )
        .await
        .expect("error-output child join should still return its canonical summary");
    assert_eq!(error_output_join["status"], "failed");
    assert_eq!(error_output_join["message"], "partial answer before provider failure");

    CloseChildAgentTool { sender: tx.clone() }
        .call(
            serde_json::json!({ "child_id": "a-completed" }),
            &CapabilityToken::default(),
        )
        .await
        .expect("close should remove completed child");
    assert!(
        call_child_status_json(&tx, "a-completed").await.is_err(),
        "closed child should be absent from status"
    );
    let after_close = call_child_roster_json(&tx)
        .await
        .expect("roster after close should succeed");
    assert!(
        after_close
            .as_array()
            .expect("roster should be an array")
            .iter()
            .all(|entry| entry["child_id"] != "a-completed"),
        "closed child should be absent from roster"
    );

    drop(join_tool);
    running_release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if call_child_status_json(&tx, "z-running")
                .await
                .expect("released running child status should succeed")["ready"]
                == true
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("released running child should finish before actor teardown");
    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor should stop after channel closes")
        .expect("actor task should shut down cleanly");
}

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
    assert_eq!(status.status, ChildAgentStatus::Running);
    assert!(!status.ready);

    let running_roster = request_child_roster(&tx).await;
    assert_eq!(running_roster.len(), 1);
    let running_child = &running_roster[0];
    assert_eq!(running_child.child_id, "child-orchestrated");
    assert_eq!(running_child.agent_type, "researcher");
    assert_eq!(running_child.task, "inspect lifecycle");
    assert_eq!(running_child.status, ChildAgentStatus::Running);
    assert!(!running_child.ready);

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

    let joined_roster = request_child_roster(&tx).await;
    assert_eq!(joined_roster.len(), 1);
    let joined_child = &joined_roster[0];
    assert_eq!(joined_child.child_id, "child-orchestrated");
    assert_eq!(joined_child.agent_type, "researcher");
    assert_eq!(joined_child.task, "inspect lifecycle");
    assert_eq!(joined_child.status, ChildAgentStatus::Completed(None));
    assert!(joined_child.ready);

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

    assert!(
        request_child_roster(&tx).await.is_empty(),
        "closed children should be absent from the supervisor roster"
    );

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
            reported_tool_uses: None,
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
            provider_content: vec![],
                    },
                    Message {
                        role: Role::Assistant,
                        content: "middle".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
            provider_content: vec![],
                    },
                    Message {
                        role: Role::Tool,
                        content: "tool two".into(),
                        tool_calls: vec![],
                        tool_call_id: Some("tool-2".into()),
            provider_content: vec![],
                    },
                    Message {
                        role: Role::Assistant,
                        content: "done".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
            provider_content: vec![],
                    },
                ],
                token_usage: TokenUsage {
                    input_tokens: 3,
                    output_tokens: 2,
                },
            reported_tool_uses: None,
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
            provider_content: vec![],
        }],
        token_usage: TokenUsage {
            input_tokens,
            output_tokens,
        },
            reported_tool_uses: None,
        used_turns: 1,
        used_cost: Decimal::ZERO,
    }
}

fn wait_any_child_budget() -> ResourceBudget {
    ResourceBudget::new(1_000, 1, Decimal::new(100, 0), 1)
}
