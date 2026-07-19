fn delivered_test_output(content: Option<&str>, exit_reason: ExitReason) -> AgentLoopOutput {
    AgentLoopOutput {
        exit_reason,
        messages: content
            .map(|content| Message {
                role: Role::Assistant,
                content: content.to_string(),
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

fn delivered_test_supervisor(
    factory: FakeTaskFactory,
) -> (
    tokio::sync::mpsc::Sender<SupervisorMessage>,
    tokio::task::JoinHandle<()>,
) {
    let supervisor = Arc::new(AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        ResourceBudget::new(100_000, 10, Decimal::new(100, 0), 0),
        Arc::new(factory),
    ));
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(rx).await });
    (tx, actor)
}

async fn spawn_delivered_test_child(
    tx: &tokio::sync::mpsc::Sender<SupervisorMessage>,
    child_id: &str,
) {
    let mut config = spawn_config(
        child_id,
        "parent-agent",
        CapabilityToken::default(),
        ResourceBudget::new(1_000, 2, Decimal::ZERO, 0),
        RestartStrategy::LetCrash,
    );
    config.agent_type = Some("worker".into());
    config.task = format!("delivery test for {child_id}");
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::Spawn(Box::new(config), result_tx),
    })
    .await
    .expect("spawn request should send");
    result_rx
        .await
        .expect("spawn acknowledgement channel should remain open")
        .expect("spawn should be accepted");
}

async fn inspect_delivered_test_child(
    tx: &tokio::sync::mpsc::Sender<SupervisorMessage>,
    child_id: &str,
) -> Result<simulacra_runtime::ChildResultInspection, String> {
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("host-control-plane".into()),
        payload: SupervisorPayload::InspectChildResult(AgentId(child_id.into()), result_tx),
    })
    .await
    .expect("host inspection request should send");
    result_rx
        .await
        .expect("host inspection response channel should remain open")
}

async fn await_delivered_test_terminal(
    tx: &tokio::sync::mpsc::Sender<SupervisorMessage>,
    child_id: &str,
) -> simulacra_runtime::ChildResultInspection {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match inspect_delivered_test_child(tx, child_id).await {
                Ok(inspection) => return inspection,
                Err(error)
                    if error.contains("still running")
                        || error.contains("not terminal")
                        || error.contains("terminal result is not ready") =>
                {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("terminal host inspection should succeed: {error}"),
            }
        }
    })
    .await
    .expect("child should become inspectable")
}

async fn stop_delivered_test_supervisor(
    tx: tokio::sync::mpsc::Sender<SupervisorMessage>,
    actor: tokio::task::JoinHandle<()>,
) {
    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("supervisor should stop after its channel closes")
        .expect("supervisor task should exit cleanly");
}

#[tokio::test]
async fn result_delivered_host_inspection_starts_false_is_stable_and_close_removes_it() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "inspect-child",
        FakeTaskPlan::Complete {
            release: None,
            output: delivered_test_output(Some("done"), ExitReason::Complete),
        },
    );
    let (tx, actor) = delivered_test_supervisor(factory);
    spawn_delivered_test_child(&tx, "inspect-child").await;

    let first = await_delivered_test_terminal(&tx, "inspect-child").await;
    let second = inspect_delivered_test_child(&tx, "inspect-child")
        .await
        .expect("repeated host inspection should succeed");
    assert!(!first.result_delivered);
    assert!(!second.result_delivered);
    assert_eq!(first.terminal.child_id.0, "inspect-child");
    assert_eq!(second.terminal.status, first.terminal.status);

    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::CloseChild(AgentId("inspect-child".into()), close_tx),
    })
    .await
    .expect("close request should send");
    close_rx.await.expect("close response should arrive").expect("close should succeed");
    assert!(inspect_delivered_test_child(&tx, "inspect-child").await.is_err());

    stop_delivered_test_supervisor(tx, actor).await;
}

#[tokio::test]
async fn result_delivered_running_probes_timeouts_and_rejected_close_do_not_mark() {
    let release = Arc::new(Notify::new());
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "running-child",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&release)),
            output: delivered_test_output(Some("later"), ExitReason::Complete),
        },
    );
    let (tx, actor) = delivered_test_supervisor(factory);
    spawn_delivered_test_child(&tx, "running-child").await;

    let (status_tx, status_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::ChildStatus(AgentId("running-child".into()), status_tx),
    })
    .await
    .expect("running status request should send");
    assert!(!status_rx.await.expect("status response should arrive").expect("status should succeed").ready);

    let roster = request_child_roster(&tx).await;
    assert_eq!(roster.len(), 1);
    assert!(!roster[0].ready);

    let (wait_tx, wait_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChild(
            AgentId("running-child".into()),
            Duration::ZERO,
            wait_tx,
        ),
    })
    .await
    .expect("running poll should send");
    assert!(!wait_rx.await.expect("poll response should arrive").expect("poll should succeed").ready);

    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::CloseChild(AgentId("running-child".into()), close_tx),
    })
    .await
    .expect("running close request should send");
    assert!(close_rx.await.expect("close response should arrive").is_err());

    release.notify_waiters();
    let inspection = await_delivered_test_terminal(&tx, "running-child").await;
    assert!(!inspection.result_delivered);

    stop_delivered_test_supervisor(tx, actor).await;
}

#[tokio::test]
async fn result_delivered_child_status_covers_all_terminal_body_variants() {
    let factory = FakeTaskFactory::new();
    let cases = [
        ("completed", Some("done"), ExitReason::Complete),
        ("null", None, ExitReason::Complete),
        ("empty", Some(""), ExitReason::Complete),
        ("cancelled", Some("cancelled body"), ExitReason::Cancelled),
        ("cancelled-null", None, ExitReason::Cancelled),
        ("failed", Some("partial"), ExitReason::Error("provider failed".into())),
    ];
    for (child_id, content, exit_reason) in &cases {
        factory.push_plan(
            child_id,
            FakeTaskPlan::Complete {
                release: None,
                output: delivered_test_output(*content, exit_reason.clone()),
            },
        );
    }
    factory.push_plan(
        "task-failed",
        FakeTaskPlan::Fail {
            error: RuntimeError::Session("task factory failed".into()),
        },
    );
    let (tx, actor) = delivered_test_supervisor(factory);

    for (child_id, _, _) in &cases {
        spawn_delivered_test_child(&tx, child_id).await;
        assert!(!await_delivered_test_terminal(&tx, child_id).await.result_delivered);

        let (status_tx, status_rx) = tokio::sync::oneshot::channel();
        tx.send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-agent".into()),
            payload: SupervisorPayload::ChildStatus(AgentId((*child_id).into()), status_tx),
        })
        .await
        .expect("terminal status request should send");
        assert!(status_rx.await.expect("status response should arrive").expect("status should succeed").ready);
        assert!(
            inspect_delivered_test_child(&tx, child_id)
                .await
                .expect("inspection after status should succeed")
                .result_delivered,
            "child_status should deliver terminal variant {child_id}"
        );
    }

    spawn_delivered_test_child(&tx, "task-failed").await;
    let failed_before = await_delivered_test_terminal(&tx, "task-failed").await;
    assert_eq!(failed_before.terminal.status, "failed");
    assert!(failed_before.terminal.result.is_err());
    assert!(!failed_before.result_delivered);
    let (status_tx, status_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::ChildStatus(AgentId("task-failed".into()), status_tx),
    })
    .await
    .expect("failed child status request should send");
    status_rx
        .await
        .expect("failed child status response should arrive")
        .expect("failed child status should be inspectable");
    assert!(
        inspect_delivered_test_child(&tx, "task-failed")
            .await
            .expect("failed child inspection after delivery")
            .result_delivered
    );

    stop_delivered_test_supervisor(tx, actor).await;
}

#[tokio::test]
async fn result_delivered_immediate_status_list_wait_and_join_each_mark_delivery() {
    for operation in ["status", "list", "wait", "join"] {
        let factory = FakeTaskFactory::new();
        factory.push_plan(
            operation,
            FakeTaskPlan::Complete {
                release: None,
                output: delivered_test_output(Some(operation), ExitReason::Complete),
            },
        );
        let (tx, actor) = delivered_test_supervisor(factory);
        spawn_delivered_test_child(&tx, operation).await;
        assert!(
            !await_delivered_test_terminal(&tx, operation)
                .await
                .result_delivered,
            "fresh terminal result should be unseen before {operation}"
        );

        match operation {
            "status" => {
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                tx.send(SupervisorMessage {
                    priority: MessagePriority::Command,
                    agent_id: AgentId("parent-agent".into()),
                    payload: SupervisorPayload::ChildStatus(
                        AgentId(operation.into()),
                        result_tx,
                    ),
                })
                .await
                .expect("status request should send");
                result_rx
                    .await
                    .expect("status response should arrive")
                    .expect("status should succeed");
            }
            "list" => {
                assert_eq!(request_child_roster(&tx).await.len(), 1);
            }
            "wait" => {
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                tx.send(SupervisorMessage {
                    priority: MessagePriority::Command,
                    agent_id: AgentId("parent-agent".into()),
                    payload: SupervisorPayload::WaitChild(
                        AgentId(operation.into()),
                        Duration::ZERO,
                        result_tx,
                    ),
                })
                .await
                .expect("wait request should send");
                result_rx
                    .await
                    .expect("wait response should arrive")
                    .expect("wait should succeed");
            }
            "join" => {
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                tx.send(SupervisorMessage {
                    priority: MessagePriority::Command,
                    agent_id: AgentId("parent-agent".into()),
                    payload: SupervisorPayload::JoinChild(
                        AgentId(operation.into()),
                        result_tx,
                    ),
                })
                .await
                .expect("join request should send");
                result_rx
                    .await
                    .expect("join response should arrive")
                    .expect("join should succeed");
            }
            _ => unreachable!(),
        }

        assert!(
            inspect_delivered_test_child(&tx, operation)
                .await
                .expect("inspection after parent delivery should succeed")
                .result_delivered,
            "{operation} should mark the terminal body delivered"
        );
        stop_delivered_test_supervisor(tx, actor).await;
    }
}

#[tokio::test]
async fn result_delivered_roster_marks_terminal_entries_but_not_running_entries() {
    let running_release = Arc::new(Notify::new());
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "a-terminal",
        FakeTaskPlan::Complete {
            release: None,
            output: delivered_test_output(Some("terminal"), ExitReason::Complete),
        },
    );
    factory.push_plan(
        "b-running",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&running_release)),
            output: delivered_test_output(Some("later"), ExitReason::Complete),
        },
    );
    factory.push_plan(
        "c-terminal",
        FakeTaskPlan::Complete {
            release: None,
            output: delivered_test_output(Some("second terminal"), ExitReason::Complete),
        },
    );
    let (tx, actor) = delivered_test_supervisor(factory);
    spawn_delivered_test_child(&tx, "a-terminal").await;
    spawn_delivered_test_child(&tx, "b-running").await;
    spawn_delivered_test_child(&tx, "c-terminal").await;
    assert!(!await_delivered_test_terminal(&tx, "a-terminal").await.result_delivered);
    assert!(!await_delivered_test_terminal(&tx, "c-terminal").await.result_delivered);

    let roster = request_child_roster(&tx).await;
    assert_eq!(roster.len(), 3);
    assert!(roster.iter().any(|entry| entry.child_id == "a-terminal" && entry.ready));
    assert!(roster.iter().any(|entry| entry.child_id == "b-running" && !entry.ready));
    assert!(roster.iter().any(|entry| entry.child_id == "c-terminal" && entry.ready));
    assert!(inspect_delivered_test_child(&tx, "a-terminal").await.expect("terminal inspection").result_delivered);
    assert!(inspect_delivered_test_child(&tx, "c-terminal").await.expect("second terminal inspection").result_delivered);

    running_release.notify_waiters();
    let running_after_completion = await_delivered_test_terminal(&tx, "b-running").await;
    assert!(!running_after_completion.result_delivered, "a running roster entry must not pre-deliver its eventual result");

    stop_delivered_test_supervisor(tx, actor).await;
}

#[tokio::test]
async fn result_delivered_pending_join_and_wait_mark_only_after_successful_send() {
    for operation in ["join", "wait"] {
        let release = Arc::new(Notify::new());
        let factory = FakeTaskFactory::new();
        factory.push_plan(
            operation,
            FakeTaskPlan::Complete {
                release: Some(Arc::clone(&release)),
                output: delivered_test_output(Some(operation), ExitReason::Complete),
            },
        );
        let (tx, actor) = delivered_test_supervisor(factory);
        spawn_delivered_test_child(&tx, operation).await;

        match operation {
            "join" => {
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                tx.send(SupervisorMessage {
                    priority: MessagePriority::Command,
                    agent_id: AgentId("parent-agent".into()),
                    payload: SupervisorPayload::JoinChild(AgentId(operation.into()), result_tx),
                })
                .await
                .expect("pending join should send");
                tokio::task::yield_now().await;
                release.notify_waiters();
                result_rx.await.expect("join response should arrive").expect("join should succeed");
            }
            "wait" => {
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                tx.send(SupervisorMessage {
                    priority: MessagePriority::Command,
                    agent_id: AgentId("parent-agent".into()),
                    payload: SupervisorPayload::WaitChild(
                        AgentId(operation.into()),
                        Duration::from_secs(1),
                        result_tx,
                    ),
                })
                .await
                .expect("pending wait should send");
                tokio::task::yield_now().await;
                release.notify_waiters();
                assert!(result_rx.await.expect("wait response should arrive").expect("wait should succeed").ready);
            }
            _ => unreachable!(),
        }

        assert!(inspect_delivered_test_child(&tx, operation).await.expect("inspection after delivery").result_delivered);
        stop_delivered_test_supervisor(tx, actor).await;
    }
}

#[tokio::test]
async fn result_delivered_wait_any_marks_only_the_selected_terminal_child() {
    let release_a = Arc::new(Notify::new());
    let release_b = Arc::new(Notify::new());
    let factory = FakeTaskFactory::new();
    for (child_id, release) in [("wait-a", &release_a), ("wait-b", &release_b)] {
        factory.push_plan(
            child_id,
            FakeTaskPlan::Complete {
                release: Some(Arc::clone(release)),
                output: delivered_test_output(Some(child_id), ExitReason::Complete),
            },
        );
    }
    let (tx, actor) = delivered_test_supervisor(factory);
    spawn_delivered_test_child(&tx, "wait-a").await;
    spawn_delivered_test_child(&tx, "wait-b").await;

    let (wait_tx, wait_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChildren(
            vec![AgentId("wait-a".into()), AgentId("wait-b".into())],
            Duration::from_secs(1),
            wait_tx,
        ),
    })
    .await
    .expect("pending wait-any should send");
    tokio::task::yield_now().await;
    release_b.notify_waiters();
    let selected = wait_rx.await.expect("wait-any response should arrive").expect("wait-any should succeed");
    assert_eq!(selected.terminal.expect("selected result").child_id.0, "wait-b");
    assert!(inspect_delivered_test_child(&tx, "wait-b").await.expect("selected inspection").result_delivered);

    release_a.notify_waiters();
    let unselected = await_delivered_test_terminal(&tx, "wait-a").await;
    assert!(!unselected.result_delivered, "wait-any must not deliver an unselected child");

    stop_delivered_test_supervisor(tx, actor).await;
}

#[tokio::test]
async fn result_delivered_immediate_wait_any_marks_only_the_selected_cached_child() {
    let factory = FakeTaskFactory::new();
    for child_id in ["cached-a", "cached-b"] {
        factory.push_plan(
            child_id,
            FakeTaskPlan::Complete {
                release: None,
                output: delivered_test_output(Some(child_id), ExitReason::Complete),
            },
        );
    }
    let (tx, actor) = delivered_test_supervisor(factory);
    spawn_delivered_test_child(&tx, "cached-a").await;
    spawn_delivered_test_child(&tx, "cached-b").await;
    assert!(!await_delivered_test_terminal(&tx, "cached-a").await.result_delivered);
    assert!(!await_delivered_test_terminal(&tx, "cached-b").await.result_delivered);

    let (wait_tx, wait_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::WaitChildren(
            vec![AgentId("cached-b".into()), AgentId("cached-a".into())],
            Duration::ZERO,
            wait_tx,
        ),
    })
    .await
    .expect("immediate wait-any should send");
    let selected = wait_rx
        .await
        .expect("immediate wait-any response should arrive")
        .expect("immediate wait-any should succeed")
        .terminal
        .expect("immediate wait-any should return one cached terminal result");
    assert_eq!(selected.child_id.0, "cached-b");
    assert!(
        inspect_delivered_test_child(&tx, "cached-b")
            .await
            .expect("selected cached child inspection")
            .result_delivered
    );
    assert!(
        !inspect_delivered_test_child(&tx, "cached-a")
            .await
            .expect("unselected cached child inspection")
            .result_delivered
    );

    stop_delivered_test_supervisor(tx, actor).await;
}

#[tokio::test]
async fn result_delivered_wait_any_running_poll_and_timeout_do_not_predeliver() {
    for timeout in [Duration::ZERO, Duration::from_millis(10)] {
        let release_a = Arc::new(Notify::new());
        let release_b = Arc::new(Notify::new());
        let factory = FakeTaskFactory::new();
        for (child_id, release) in [("running-a", &release_a), ("running-b", &release_b)] {
            factory.push_plan(
                child_id,
                FakeTaskPlan::Complete {
                    release: Some(Arc::clone(release)),
                    output: delivered_test_output(Some(child_id), ExitReason::Complete),
                },
            );
        }
        let (tx, actor) = delivered_test_supervisor(factory);
        spawn_delivered_test_child(&tx, "running-a").await;
        spawn_delivered_test_child(&tx, "running-b").await;

        let (wait_tx, wait_rx) = tokio::sync::oneshot::channel();
        tx.send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-agent".into()),
            payload: SupervisorPayload::WaitChildren(
                vec![AgentId("running-a".into()), AgentId("running-b".into())],
                timeout,
                wait_tx,
            ),
        })
        .await
        .expect("running wait-any should send");
        let running = wait_rx
            .await
            .expect("running wait-any response should arrive")
            .expect("running wait-any should succeed");
        assert!(!running.ready);
        assert!(running.terminal.is_none());

        release_a.notify_one();
        release_b.notify_one();
        assert!(!await_delivered_test_terminal(&tx, "running-a").await.result_delivered);
        assert!(!await_delivered_test_terminal(&tx, "running-b").await.result_delivered);
        stop_delivered_test_supervisor(tx, actor).await;
    }
}

#[tokio::test]
async fn result_delivered_dropped_wait_any_channels_do_not_mark_selected_results() {
    for pending in [false, true] {
        let selected_release = Arc::new(Notify::new());
        let factory = FakeTaskFactory::new();
        factory.push_plan(
            "wait-any-drop",
            FakeTaskPlan::Complete {
                release: pending.then(|| Arc::clone(&selected_release)),
                output: delivered_test_output(Some("undelivered"), ExitReason::Complete),
            },
        );
        let (tx, actor) = delivered_test_supervisor(factory);
        spawn_delivered_test_child(&tx, "wait-any-drop").await;
        if !pending {
            assert!(
                !await_delivered_test_terminal(&tx, "wait-any-drop")
                    .await
                    .result_delivered
            );
        }

        let (wait_tx, wait_rx) = tokio::sync::oneshot::channel();
        drop(wait_rx);
        tx.send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-agent".into()),
            payload: SupervisorPayload::WaitChildren(
                vec![AgentId("wait-any-drop".into())],
                Duration::from_secs(1),
                wait_tx,
            ),
        })
        .await
        .expect("dropped wait-any request should send");
        tokio::task::yield_now().await;
        if pending {
            selected_release.notify_one();
        }
        assert!(
            !await_delivered_test_terminal(&tx, "wait-any-drop")
                .await
                .result_delivered,
            "failed wait-any response send must not mark its selected child"
        );
        let (recovery_tx, recovery_rx) = tokio::sync::oneshot::channel();
        tx.send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-agent".into()),
            payload: SupervisorPayload::JoinChild(AgentId("wait-any-drop".into()), recovery_tx),
        })
        .await
        .expect("wait-any recovery join should send");
        recovery_rx
            .await
            .expect("wait-any recovery response should arrive")
            .expect("wait-any recovery join should succeed");
        assert!(
            inspect_delivered_test_child(&tx, "wait-any-drop")
                .await
                .expect("inspection after wait-any recovery")
                .result_delivered
        );
        stop_delivered_test_supervisor(tx, actor).await;
    }
}

#[tokio::test]
async fn result_delivered_dropped_result_channels_do_not_mark_terminal_results() {
    for operation in ["status", "list", "wait", "join", "pending-join", "pending-wait"] {
        let release = Arc::new(Notify::new());
        let pending = operation.starts_with("pending-");
        let factory = FakeTaskFactory::new();
        factory.push_plan(
            operation,
            FakeTaskPlan::Complete {
                release: pending.then(|| Arc::clone(&release)),
                output: delivered_test_output(Some(operation), ExitReason::Complete),
            },
        );
        let (tx, actor) = delivered_test_supervisor(factory);
        spawn_delivered_test_child(&tx, operation).await;
        if !pending {
            assert!(!await_delivered_test_terminal(&tx, operation).await.result_delivered);
        }

        match operation {
            "status" => {
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                drop(result_rx);
                tx.send(SupervisorMessage {
                    priority: MessagePriority::Command,
                    agent_id: AgentId("parent-agent".into()),
                    payload: SupervisorPayload::ChildStatus(AgentId(operation.into()), result_tx),
                }).await.expect("dropped status request should send");
            }
            "list" => {
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                drop(result_rx);
                tx.send(SupervisorMessage {
                    priority: MessagePriority::Command,
                    agent_id: AgentId("parent-agent".into()),
                    payload: SupervisorPayload::ListChildren(result_tx),
                }).await.expect("dropped list request should send");
            }
            "wait" | "pending-wait" => {
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                drop(result_rx);
                tx.send(SupervisorMessage {
                    priority: MessagePriority::Command,
                    agent_id: AgentId("parent-agent".into()),
                    payload: SupervisorPayload::WaitChild(
                        AgentId(operation.into()),
                        Duration::from_secs(1),
                        result_tx,
                    ),
                }).await.expect("dropped wait request should send");
            }
            "join" | "pending-join" => {
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                drop(result_rx);
                tx.send(SupervisorMessage {
                    priority: MessagePriority::Command,
                    agent_id: AgentId("parent-agent".into()),
                    payload: SupervisorPayload::JoinChild(AgentId(operation.into()), result_tx),
                }).await.expect("dropped join request should send");
            }
            _ => unreachable!(),
        }
        tokio::task::yield_now().await;
        if pending {
            release.notify_waiters();
        }
        let inspection = await_delivered_test_terminal(&tx, operation).await;
        assert!(!inspection.result_delivered, "failed {operation} response send must not deliver");

        let (recovery_tx, recovery_rx) = tokio::sync::oneshot::channel();
        tx.send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-agent".into()),
            payload: SupervisorPayload::JoinChild(AgentId(operation.into()), recovery_tx),
        })
        .await
        .expect("recovery join should send");
        recovery_rx
            .await
            .expect("recovery join response should arrive")
            .expect("recovery join should succeed after a failed response send");
        assert!(
            inspect_delivered_test_child(&tx, operation)
                .await
                .expect("inspection after recovery delivery")
                .result_delivered,
            "a later successful delivery must transition after failed {operation} send"
        );
        stop_delivered_test_supervisor(tx, actor).await;
    }
}

#[tokio::test]
async fn result_delivered_pending_receivers_observe_a_linearized_monotonic_cached_result() {
    let release = Arc::new(Notify::new());
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "concurrent-child",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&release)),
            output: delivered_test_output(Some("one cached body"), ExitReason::Complete),
        },
    );
    let (tx, actor) = delivered_test_supervisor(factory);
    spawn_delivered_test_child(&tx, "concurrent-child").await;

    let mut receivers = Vec::new();
    for _ in 0..32 {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        tx.send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-agent".into()),
            payload: SupervisorPayload::JoinChild(AgentId("concurrent-child".into()), result_tx),
        })
        .await
        .expect("concurrent join request should send");
        receivers.push(result_rx);
    }
    let (running_tx, running_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::ChildStatus(AgentId("concurrent-child".into()), running_tx),
    })
    .await
    .expect("running barrier status should send");
    assert!(
        !running_rx
            .await
            .expect("running barrier status response should arrive")
            .expect("running barrier status should succeed")
            .ready,
        "all earlier pending joins should be registered before completion"
    );

    release.notify_one();
    let first_terminal = receivers
        .remove(0)
        .await
        .expect("first join response should arrive")
        .expect("first join should succeed");
    assert_eq!(first_terminal.child_id.0, "concurrent-child");
    assert_eq!(first_terminal.result.as_ref().expect("cached success").messages[0].content, "one cached body");
    assert!(
        inspect_delivered_test_child(&tx, "concurrent-child")
            .await
            .expect("inspection immediately after receiver observes delivery")
            .result_delivered,
        "delivery state must be committed before a successful receiver can observe the body"
    );

    for receiver in receivers {
        let terminal = receiver
            .await
            .expect("join response should arrive")
            .expect("join should succeed");
        assert_eq!(terminal.child_id.0, "concurrent-child");
        assert_eq!(terminal.result.as_ref().expect("cached success").messages[0].content, "one cached body");
    }

    for _ in 0..16 {
        let inspection = inspect_delivered_test_child(&tx, "concurrent-child")
            .await
            .expect("repeated concurrent aftermath inspection should succeed");
        assert!(inspection.result_delivered, "delivery must never regress to false");
        assert_eq!(inspection.terminal.child_id.0, "concurrent-child");
        assert_eq!(
            inspection
                .terminal
                .result
                .as_ref()
                .expect("cached inspection success")
                .messages[0]
                .content,
            "one cached body",
            "delivery and repeated inspection must not consume or alter the cached body"
        );
    }

    stop_delivered_test_supervisor(tx, actor).await;
}

#[tokio::test]
#[cfg(feature = "spawn")]
async fn result_delivered_model_visible_json_remains_unchanged() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "json-child",
        FakeTaskPlan::Complete {
            release: None,
            output: delivered_test_output(Some("json body"), ExitReason::Complete),
        },
    );
    let (tx, actor) = delivered_test_supervisor(factory);
    spawn_delivered_test_child(&tx, "json-child").await;
    assert!(!await_delivered_test_terminal(&tx, "json-child").await.result_delivered);

    let values = vec![
        ChildStatusTool { sender: tx.clone() }
            .call(serde_json::json!({"child_id": "json-child"}), &CapabilityToken::default())
            .await
            .expect("status tool should succeed"),
        ListChildAgentTool { sender: tx.clone() }
            .call(serde_json::json!({}), &CapabilityToken::default())
            .await
            .expect("list tool should succeed"),
        WaitChildAgentTool { sender: tx.clone() }
            .call(
                serde_json::json!({"child_id": "json-child", "timeout_ms": 0}),
                &CapabilityToken::default(),
            )
            .await
            .expect("wait tool should succeed"),
        JoinChildAgentTool { sender: tx.clone() }
            .call(serde_json::json!({"child_id": "json-child"}), &CapabilityToken::default())
            .await
            .expect("join tool should succeed"),
    ];
    for value in values {
        let encoded = serde_json::to_string(&value).expect("tool JSON should encode");
        assert!(!encoded.contains("result_delivered"));
        assert!(!encoded.contains("result_observed"));
    }

    stop_delivered_test_supervisor(tx, actor).await;
}

async fn inspect_delivered_test_roster(
    tx: &tokio::sync::mpsc::Sender<SupervisorMessage>,
) -> Vec<simulacra_runtime::ChildRosterEntry> {
    let (list_tx, list_rx) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("host-control-plane".into()),
        payload: SupervisorPayload::InspectChildren(list_tx),
    })
    .await
    .expect("host roster inspection request should send");
    list_rx
        .await
        .expect("host roster inspection response channel should stay open")
        .expect("host roster inspection should succeed")
}

#[tokio::test]
async fn result_delivered_host_roster_inspection_never_marks_delivery() {
    let running_release = Arc::new(Notify::new());
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "a-terminal",
        FakeTaskPlan::Complete {
            release: None,
            output: delivered_test_output(Some("terminal"), ExitReason::Complete),
        },
    );
    factory.push_plan(
        "b-running",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&running_release)),
            output: delivered_test_output(Some("later"), ExitReason::Complete),
        },
    );
    let (tx, actor) = delivered_test_supervisor(factory);
    spawn_delivered_test_child(&tx, "a-terminal").await;
    spawn_delivered_test_child(&tx, "b-running").await;
    assert!(
        !await_delivered_test_terminal(&tx, "a-terminal")
            .await
            .result_delivered
    );

    // Host-only roster inspection sees the same entries a model-facing list
    // would, but never acknowledges the handoff: a host housekeeping sweep
    // (e.g. end-of-turn supervisor teardown) must not disarm a pending
    // terminal-result delivery to the parent model.
    let inspected = inspect_delivered_test_roster(&tx).await;
    assert_eq!(
        inspected
            .iter()
            .map(|entry| entry.child_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a-terminal", "b-running"],
        "host inspection must use the model roster's deterministic child_id order"
    );
    assert!(
        !inspect_delivered_test_child(&tx, "a-terminal")
            .await
            .expect("terminal inspection")
            .result_delivered,
        "host roster inspection must not mark a terminal result delivered"
    );

    // Repeated host inspections stay non-consuming.
    let _ = inspect_delivered_test_roster(&tx).await;
    assert!(
        !inspect_delivered_test_child(&tx, "a-terminal")
            .await
            .expect("terminal inspection after repeat")
            .result_delivered
    );

    // The model-facing roster keeps its delivery-acknowledging behavior.
    let roster = request_child_roster(&tx).await;
    assert_eq!(inspected.len(), roster.len());
    for (inspected_entry, model_entry) in inspected.iter().zip(&roster) {
        assert_eq!(inspected_entry.child_id, model_entry.child_id);
        assert_eq!(inspected_entry.agent_type, model_entry.agent_type);
        assert_eq!(inspected_entry.task, model_entry.task);
        assert_eq!(inspected_entry.status, model_entry.status);
        assert_eq!(inspected_entry.ready, model_entry.ready);
        assert!(
            model_entry.elapsed_ms >= inspected_entry.elapsed_ms,
            "the later model roster snapshot must not report an earlier elapsed time"
        );
    }
    assert!(
        inspect_delivered_test_child(&tx, "a-terminal")
            .await
            .expect("terminal inspection after model list")
            .result_delivered,
        "model-facing list_children must still mark terminal results delivered"
    );

    running_release.notify_waiters();
    let released = await_delivered_test_terminal(&tx, "b-running").await;
    assert!(
        !released.result_delivered,
        "a running entry inspected by the host must not pre-deliver its eventual result"
    );

    stop_delivered_test_supervisor(tx, actor).await;
}
