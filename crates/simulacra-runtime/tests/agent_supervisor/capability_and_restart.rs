#[test]
fn supervisor_enforces_capability_attenuation_on_spawn() {
    let parent_capability = CapabilityToken::default();
    let child_capability = CapabilityToken {
        shell: true,
        ..CapabilityToken::default()
    };
    let mut supervisor = AgentSupervisor::new(parent_capability, default_budget());

    let err = supervisor
        .spawn_agent(spawn_config(
            "child-agent",
            "parent-agent",
            child_capability,
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect_err("spawn_agent should reject child capabilities that exceed the parent token");

    assert!(
        matches!(err, RuntimeError::CapabilityViolation(ref message) if message.contains("subset")),
        "expected a capability violation when the child requests shell access the parent lacks, got {err:?}"
    );
}

// S009 Assertion: Restart strategy is applied on agent failure.
#[test]
fn restart_strategy_is_applied_on_agent_failure() {
    let supervisor = AgentSupervisor::new(CapabilityToken::default(), default_budget());

    let retry_once_agent = AgentId("retry-once".into());
    assert!(
        supervisor.handle_failure(
            &retry_once_agent,
            &RestartStrategy::RetryOnce,
            "first failure",
        ),
        "retry_once should restart the agent on its first failure"
    );
    assert!(
        !supervisor.handle_failure(
            &retry_once_agent,
            &RestartStrategy::RetryOnce,
            "second failure",
        ),
        "retry_once should stop restarting after consuming the single retry"
    );

    let retry_twice_agent = AgentId("retry-twice".into());
    assert!(
        supervisor.handle_failure(
            &retry_twice_agent,
            &RestartStrategy::RetryTwiceThenFail,
            "first failure",
        ),
        "retry_twice_then_fail should restart the agent on the first failure"
    );
    assert!(
        supervisor.handle_failure(
            &retry_twice_agent,
            &RestartStrategy::RetryTwiceThenFail,
            "second failure",
        ),
        "retry_twice_then_fail should restart the agent on the second failure"
    );
    assert!(
        !supervisor.handle_failure(
            &retry_twice_agent,
            &RestartStrategy::RetryTwiceThenFail,
            "third failure",
        ),
        "retry_twice_then_fail should stop restarting after the second retry"
    );

    let snapshot_agent = AgentId("snapshot-agent".into());
    assert!(
        !supervisor.handle_failure(
            &snapshot_agent,
            &RestartStrategy::SnapshotAndFail,
            "snapshot failure",
        ),
        "snapshot_and_fail should propagate the failure instead of retrying"
    );

    let let_crash_agent = AgentId("let-crash-agent".into());
    assert!(
        !supervisor.handle_failure(&let_crash_agent, &RestartStrategy::LetCrash, "boom"),
        "let_crash should propagate the failure without retrying"
    );
}

// S009 Assertion: Cancelled agent receives cancellation signal.
#[tokio::test]
async fn cancelled_agent_receives_cancellation_signal() {
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(NoopTaskFactory),
    );
    let token = supervisor
        .spawn_agent(spawn_config(
            "cancelled-agent",
            "parent-agent",
            CapabilityToken::default(),
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect("spawning a child with inherited capabilities should succeed");

    let observed_cancellation = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_cancellation_clone = Arc::clone(&observed_cancellation);
    let task_token = token.clone();

    let child_task = tokio::spawn(async move {
        while !task_token.is_cancelled() {
            tokio::task::yield_now().await;
        }
        observed_cancellation_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    supervisor.cancel_agent(&token);

    tokio::time::timeout(token.grace(), async {
        child_task
            .await
            .expect("the child task should complete after observing cancellation");
    })
    .await
    .expect("the child task should observe cancellation before the grace period expires");

    assert!(
        observed_cancellation.load(std::sync::atomic::Ordering::SeqCst),
        "expected the child task to observe the supervisor's cancellation signal"
    );
}

// S009 Assertion: Child budget does not exceed parent budget.
#[test]
fn child_budget_does_not_exceed_parent_budget() {
    let parent_capability = CapabilityToken::default();
    let mut parent_budget = default_budget();
    parent_budget.max_tokens = 10;
    parent_budget.used_tokens = 5;
    let mut supervisor = AgentSupervisor::new(parent_capability, parent_budget);

    let child_budget = ResourceBudget {
        max_tokens: 6,
        ..default_budget()
    };

    let err = supervisor
        .spawn_agent(spawn_config(
            "child-agent",
            "parent-agent",
            CapabilityToken::default(),
            child_budget,
            RestartStrategy::LetCrash,
        ))
        .expect_err("spawn_agent should reject a child budget that exceeds the parent's remaining token budget");

    assert!(
        matches!(err, RuntimeError::BudgetExhausted(ref exhausted) if exhausted.resource == "tokens"),
        "expected spawn_agent to reject the oversized child token budget, got {err:?}"
    );
}

// S009 O11y Assertion: Agent spawn produces a create_agent span with gen_ai.agent.name.
#[test]
fn agent_spawn_produces_create_agent_span_with_agent_name() {
    let (subscriber, captured_spans, _captured_events) = setup_capture();
    let dispatch = tracing::Dispatch::new(subscriber);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime should build");
    let _runtime_guard = runtime.enter();
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(NoopTaskFactory),
    );

    tracing::dispatcher::with_default(&dispatch, || {
        supervisor
            .spawn_agent(spawn_config(
                "child-agent",
                "parent-agent",
                CapabilityToken::default(),
                default_budget(),
                RestartStrategy::LetCrash,
            ))
            .expect("spawning a child with inherited capabilities should succeed");
    });

    let spans = captured_spans.lock().unwrap();
    let create_agent_span = spans
        .iter()
        .find(|span| {
            span.name == "create_agent"
                && span.fields.get("gen_ai.operation.name") == Some(&"create_agent".to_string())
        })
        .expect("expected a create_agent span to be emitted during spawn");

    assert_eq!(
        create_agent_span.fields.get("gen_ai.agent.name"),
        Some(&"child-agent".to_string())
    );
}

// S009 O11y Assertion: Agent invocation is wrapped in an invoke_agent span.
#[tokio::test]
async fn agent_invocation_is_wrapped_in_invoke_agent_span() {
    let (subscriber, captured_spans, _captured_events) = setup_capture();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![text_response("done")]);
    let mut agent = build_loop(provider, ToolRegistry::new(), journal, default_budget());

    let (_capture_guard, _guard) = install_capture(subscriber).await;
    let _output = agent.run("say hello").await.expect("run should succeed");

    let spans = captured_spans.lock().unwrap();
    let invoke_agent_span = spans
        .iter()
        .find(|span| span.fields.get("gen_ai.operation.name") == Some(&"invoke_agent".to_string()))
        .expect("expected a span with gen_ai.operation.name=invoke_agent");

    assert_eq!(
        invoke_agent_span.fields.get("gen_ai.agent.name"),
        Some(&"test-agent".to_string())
    );
}

// S009 O11y Assertion: simulacra.agent.turns counter tracks turns per agent.
#[tokio::test]
async fn simulacra_agent_turns_counter_tracks_turns_per_agent() {
    let (subscriber, _captured_spans, captured_events) = setup_capture();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![
        tool_call_response("echo", serde_json::json!({ "msg": "hi" })),
        text_response("done"),
    ]);
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(EchoTool))
        .expect("test tool registration should succeed");
    let mut agent = build_loop(provider, tools, journal, default_budget());

    let (_capture_guard, _guard) = install_capture(subscriber).await;
    let _output = agent
        .run("use the echo tool and finish")
        .await
        .expect("run should succeed");

    let events = captured_events.lock().unwrap();
    let turn_events = events
        .iter()
        .filter(|event| event.fields.get("simulacra.agent.turns") == Some(&"1".to_string()))
        .collect::<Vec<_>>();

    assert_eq!(
        turn_events.len(),
        2,
        "expected simulacra.agent.turns to be emitted once per turn for the current agent"
    );
    assert!(
        turn_events
            .iter()
            .all(|event| event.current_span.as_deref() == Some("invoke_agent")),
        "turn metrics should be emitted on the invoke_agent span for per-agent attribution"
    );
}

// S009 O11y Assertion: Agent spawn is logged at INFO with agent name, parent, and capabilities.
#[tokio::test]
async fn agent_spawn_is_logged_at_info_with_agent_name_parent_and_capabilities() {
    let (subscriber, _captured_spans, captured_events) = setup_capture();
    let child_capability = CapabilityToken {
        shell: true,
        ..CapabilityToken::default()
    };
    let mut supervisor = AgentSupervisor::with_task_factory(
        child_capability.clone(),
        default_budget(),
        Arc::new(NoopTaskFactory),
    );

    let (_capture_guard, _guard) = install_capture(subscriber).await;
    supervisor
        .spawn_agent(spawn_config(
            "child-agent",
            "parent-agent",
            child_capability,
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect("spawning a child with inherited capabilities should succeed");

    let events = captured_events.lock().unwrap();
    let spawn_event = events
        .iter()
        .find(|event| {
            event.level == "INFO"
                && event.current_span.as_deref() == Some("create_agent")
                && event.fields.get("gen_ai.agent.name") == Some(&"child-agent".to_string())
                && event.fields.get("parent") == Some(&"parent-agent".to_string())
        })
        .expect("expected an INFO spawn event with agent and parent context");

    assert!(
        spawn_event
            .fields
            .get("capabilities")
            .is_some_and(|value| value.contains("shell: true")),
        "expected the spawn log to include the child's capabilities, got {:?}",
        spawn_event.fields
    );
}

// S009 O11y Assertion: Agent completion is logged at INFO with agent name, exit reason, and token total.
#[tokio::test]
async fn agent_completion_is_logged_at_info_with_agent_name_exit_reason_and_token_total() {
    let (subscriber, _captured_spans, captured_events) = setup_capture();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![text_response("done")]);
    let mut agent = build_loop(provider, ToolRegistry::new(), journal, default_budget());

    let (_capture_guard, _guard) = install_capture(subscriber).await;
    let output = agent.run("say hello").await.expect("run should succeed");

    let events = captured_events.lock().unwrap();
    let completion_event = events
        .iter()
        .find(|event| {
            event.level == "INFO"
                && event.current_span.as_deref() == Some("invoke_agent")
                && event.fields.get("gen_ai.agent.name") == Some(&"test-agent".to_string())
                && event.fields.get("simulacra.agent.exit_reason") == Some(&"Complete".to_string())
                && event.fields.get("simulacra.agent.token_total")
                    == Some(&output.token_usage.total().to_string())
        })
        .expect(
            "expected an INFO completion event with the agent name, exit reason, and token total",
        );

    assert_eq!(
        completion_event.fields.get("simulacra.agent.token_total"),
        Some(&output.token_usage.total().to_string())
    );
}

// S009 O11y Assertion: Agent restart is logged at WARN with agent name, strategy, and failure reason.
#[tokio::test]
async fn agent_restart_is_logged_at_warn_with_agent_name_strategy_and_failure_reason() {
    let (subscriber, _captured_spans, captured_events) = setup_capture();
    let supervisor = AgentSupervisor::new(CapabilityToken::default(), default_budget());
    let agent_id = AgentId("restarting-child".into());

    let (_capture_guard, _guard) = install_capture(subscriber).await;
    assert!(
        supervisor.handle_failure(&agent_id, &RestartStrategy::RetryOnce, "boom"),
        "retry_once should request a restart on the first failure"
    );

    let events = captured_events.lock().unwrap();
    let restart_event = events
        .iter()
        .find(|event| {
            event.level == "WARN"
                && event.fields.get("gen_ai.agent.name") == Some(&"restarting-child".to_string())
                && event.fields.get("strategy") == Some(&"retry_once".to_string())
                && event.fields.get("failure_reason") == Some(&"boom".to_string())
        })
        .expect("expected a WARN restart event with agent name, strategy, and failure reason");

    assert_eq!(
        restart_event.fields.get("strategy"),
        Some(&"retry_once".to_string())
    );
}

// S009 Assertion: retry_once strategy restarts the agent exactly once then fails.
#[test]
fn retry_once_restarts_exactly_once_then_fails() {
    let supervisor = AgentSupervisor::new(CapabilityToken::default(), default_budget());
    let agent_id = AgentId("retry-once-child".into());

    assert!(
        supervisor.handle_failure(&agent_id, &RestartStrategy::RetryOnce, "first failure"),
        "retry_once should restart the child on the first failure"
    );
    assert!(
        !supervisor.handle_failure(&agent_id, &RestartStrategy::RetryOnce, "second failure"),
        "retry_once should stop restarting the child after the single retry is consumed"
    );
}

// S009 Assertion: retry_twice_then_fail strategy restarts at most twice.
#[test]
fn retry_twice_then_fail_restarts_at_most_twice() {
    let supervisor = AgentSupervisor::new(CapabilityToken::default(), default_budget());
    let agent_id = AgentId("retry-twice-child".into());

    assert!(
        supervisor.handle_failure(
            &agent_id,
            &RestartStrategy::RetryTwiceThenFail,
            "first failure",
        ),
        "retry_twice_then_fail should restart the child on the first failure"
    );
    assert!(
        supervisor.handle_failure(
            &agent_id,
            &RestartStrategy::RetryTwiceThenFail,
            "second failure",
        ),
        "retry_twice_then_fail should restart the child on the second failure"
    );
    assert!(
        !supervisor.handle_failure(
            &agent_id,
            &RestartStrategy::RetryTwiceThenFail,
            "third failure",
        ),
        "retry_twice_then_fail should stop restarting the child after the second retry is consumed"
    );
}

// S009 Assertion: let_crash does not restart the child.
#[tokio::test]
async fn let_crash_does_not_restart() {
    let (subscriber, _captured_spans, captured_events) = setup_capture();
    let supervisor = AgentSupervisor::new(CapabilityToken::default(), default_budget());
    let agent_id = AgentId("let-crash-child".into());

    let (_capture_guard, _guard) = install_capture(subscriber).await;
    let should_restart = supervisor.handle_failure(&agent_id, &RestartStrategy::LetCrash, "boom");

    assert!(
        !should_restart,
        "let_crash should not schedule a restart for a failed child"
    );

    let events = captured_events.lock().unwrap();
    assert!(
        events.iter().all(|event| {
            !(event.level == "WARN"
                && event.fields.get("gen_ai.agent.name") == Some(&"let-crash-child".to_string())
                && event.fields.get("message") == Some(&"agent restart triggered".to_string()))
        }),
        "let_crash should not emit a restart warning when the strategy is to let the child crash"
    );
}

// S009 RED Assertion: spawn_agent should start a child task and observe its completion.
