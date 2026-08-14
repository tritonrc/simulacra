fn s060_assert_placement_result(value: &serde_json::Value, placement: &str) {
    assert_eq!(
        value["placement"], placement,
        "missing placement in {value}"
    );
    assert!(
        value.get("agent_type").is_none(),
        "legacy agent_type in {value}"
    );
}

fn s060_keys(value: &serde_json::Value) -> std::collections::BTreeSet<&str> {
    value
        .as_object()
        .expect("result should be an object")
        .keys()
        .map(String::as_str)
        .collect()
}

struct S060LifecycleSink(Arc<Mutex<Vec<simulacra_types::ActivityEvent>>>);

impl simulacra_runtime::ActivitySink for S060LifecycleSink {
    fn emit(&self, event: simulacra_types::ActivityEvent) {
        self.0.lock().expect("activity event lock").push(event);
    }
}

struct S060ConfiguredCountingFactory {
    placement: &'static str,
    validate_calls: Arc<AtomicUsize>,
    create_calls: Arc<AtomicUsize>,
}

impl TaskFactory for S060ConfiguredCountingFactory {
    fn validate_spawn_config(&self, config: &SpawnConfig) -> Result<(), RuntimeError> {
        self.validate_calls.fetch_add(1, Ordering::SeqCst);
        if config.placement == self.placement {
            Ok(())
        } else {
            Err(RuntimeError::CapabilityViolation(format!(
                "unknown child placement {:?}; available placements: {}",
                config.placement, self.placement
            )))
        }
    }

    fn placement_backend(&self, _config: &SpawnConfig) -> AgentBackend {
        AgentBackend::Native
    }

    fn create_task(&self, _config: SpawnConfig, _token: CancellationToken) -> BoxTaskFuture {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(child_success_output()) })
    }
}

#[tokio::test]
async fn s060_a31_spawn_acknowledgement_uses_exact_placement_shape() {
    let (tool, receiver) = s060_spawn_tool(
        &["workspace"],
        ResourceBudget::new(0, 0, Decimal::ZERO, 0),
        None,
    );
    let (result, dispatched) = s060_call_and_capture(
        &tool,
        receiver,
        serde_json::json!({
            "placement": "workspace",
            "instructions": "  preserve me \n",
            "task": "bounded work",
            "budget": s060_budget(1, 1, "0", 0)
        }),
        &s060_capability(&["workspace"]),
    )
    .await;

    let acknowledgement = result.expect("valid S060 spawn should be acknowledged");
    assert!(dispatched.is_some());
    let object = acknowledgement.as_object().expect("acknowledgement object");
    assert_eq!(
        object
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        ["child_id", "placement", "status"].into_iter().collect()
    );
    assert_eq!(acknowledgement["placement"], "workspace");
    assert_eq!(acknowledgement["status"], "running");
}

#[tokio::test]
async fn s060_a31_status_list_wait_and_join_results_use_placement() {
    let child_id = AgentId("child-0123456789abcdef0123456789abcdef".into());
    let capability = CapabilityToken::default();

    let (status_tx, mut status_rx) = tokio::sync::mpsc::channel(1);
    let status_tool = ChildStatusTool {
        sender: status_tx,
        caller_id: AgentId("parent-agent".into()),
    };
    let status_call = status_tool.call(serde_json::json!({"child_id": child_id.0}), &capability);
    let status_reply = async {
        let message = status_rx.recv().await.expect("status request");
        let SupervisorPayload::ChildStatus(id, reply) = message.payload else {
            panic!("expected ChildStatus")
        };
        reply
            .send(Ok(simulacra_runtime::ChildStatus {
                child_id: id,
                placement: "workspace".into(),
                status: ChildAgentStatus::Running,
                ready: false,
                elapsed_ms: 4,
            }))
            .expect("status caller should remain live");
    };
    let (status, ()) = tokio::join!(status_call, status_reply);
    let status = status.expect("status result");
    s060_assert_placement_result(&status, "workspace");
    assert_eq!(status["child_id"], child_id.0);
    assert_eq!(status["status"], "running");
    assert_eq!(status["ready"], false);
    assert_eq!(status["elapsed_ms"], 4);
    assert_eq!(
        s060_keys(&status),
        ["child_id", "elapsed_ms", "placement", "ready", "status"]
            .into_iter()
            .collect()
    );

    let (list_tx, mut list_rx) = tokio::sync::mpsc::channel(1);
    let list_tool = ListChildAgentTool {
        sender: list_tx,
        caller_id: AgentId("parent-agent".into()),
    };
    let list_call = list_tool.call(serde_json::json!({}), &capability);
    let list_reply = async {
        let message = list_rx.recv().await.expect("list request");
        let SupervisorPayload::ListChildren(reply) = message.payload else {
            panic!("expected ListChildren")
        };
        reply
            .send(Ok(vec![simulacra_runtime::ChildRosterEntry {
                child_id: child_id.0.clone(),
                placement: "workspace".into(),
                task: "bounded work".into(),
                status: ChildAgentStatus::Running,
                ready: false,
                elapsed_ms: 4,
            }]))
            .expect("list caller should remain live");
    };
    let (list, ()) = tokio::join!(list_call, list_reply);
    let list = list.expect("list result");
    s060_assert_placement_result(&list[0], "workspace");
    assert_eq!(list[0]["child_id"], child_id.0);
    assert_eq!(list[0]["task"], "bounded work");
    assert_eq!(list[0]["status"], "running");
    assert_eq!(list[0]["ready"], false);
    assert_eq!(list[0]["elapsed_ms"], 4);
    assert_eq!(
        s060_keys(&list[0]),
        [
            "child_id",
            "elapsed_ms",
            "placement",
            "ready",
            "status",
            "task",
        ]
        .into_iter()
        .collect()
    );

    let (wait_tx, mut wait_rx) = tokio::sync::mpsc::channel(1);
    let wait_tool = WaitChildAgentTool {
        sender: wait_tx,
        caller_id: AgentId("parent-agent".into()),
    };
    let wait_call = wait_tool.call(
        serde_json::json!({"child_id": child_id.0, "timeout_ms": 0}),
        &capability,
    );
    let wait_reply = async {
        let message = wait_rx.recv().await.expect("wait request");
        let SupervisorPayload::WaitChild(id, _timeout, reply) = message.payload else {
            panic!("expected WaitChild")
        };
        reply
            .send(Ok(simulacra_runtime::WaitChildResult {
                child_id: id,
                placement: Some("workspace".into()),
                status: "running".into(),
                ready: false,
                terminal: None,
            }))
            .expect("wait caller should remain live");
    };
    let (wait, ()) = tokio::join!(wait_call, wait_reply);
    let wait = wait.expect("wait result");
    s060_assert_placement_result(&wait, "workspace");
    assert_eq!(wait["child_id"], child_id.0);
    assert_eq!(wait["status"], "running");
    assert_eq!(wait["ready"], false);
    assert_eq!(
        s060_keys(&wait),
        ["child_id", "placement", "ready", "status"]
            .into_iter()
            .collect()
    );

    let (join_tx, mut join_rx) = tokio::sync::mpsc::channel(1);
    let join_tool = JoinChildAgentTool {
        sender: join_tx,
        caller_id: AgentId("parent-agent".into()),
    };
    let join_call = join_tool.call(serde_json::json!({"child_id": child_id.0}), &capability);
    let join_reply = async {
        let message = join_rx.recv().await.expect("join request");
        let SupervisorPayload::JoinChild(id, reply) = message.payload else {
            panic!("expected JoinChild")
        };
        reply
            .send(Ok(ChildTerminalResult {
                child_id: id,
                placement: "workspace".into(),
                status: "completed".into(),
                elapsed_ms: 8,
                tool_uses: 0,
                result: Ok(child_success_output()),
            }))
            .expect("join caller should remain live");
    };
    let (join, ()) = tokio::join!(join_call, join_reply);
    let join = join.expect("join result");
    s060_assert_placement_result(&join, "workspace");
    assert_eq!(join["child_id"], child_id.0);
    assert_eq!(join["status"], "completed");
    assert_eq!(join["ready"], true);
    assert_eq!(join["exit_reason"], "completed");
    assert_eq!(join["message"], "child summary");
    assert_eq!(join["elapsed_ms"], 8);
    assert_eq!(join["tool_uses"], 0);
    assert_eq!(join["token_usage"]["input_tokens"], 3);
    assert_eq!(join["token_usage"]["output_tokens"], 2);
    assert_eq!(join["artifacts"], serde_json::json!([]));
    assert_eq!(join["vfs_changes"], serde_json::json!([]));
    assert_eq!(
        s060_keys(&join),
        [
            "artifacts",
            "child_id",
            "elapsed_ms",
            "exit_reason",
            "message",
            "placement",
            "ready",
            "status",
            "token_usage",
            "tool_uses",
            "vfs_changes",
        ]
        .into_iter()
        .collect()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s060_a32_one_thousand_concurrent_spawns_receive_opaque_unique_ids() {
    let parent_budget = Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0)));
    let capability = s060_capability(&["workspace"]);
    let parent_id = AgentId("one-root-s060-session".into());
    let journal = Arc::new(InMemoryJournalStorage::new());
    let validate_calls = Arc::new(AtomicUsize::new(0));
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&parent_budget),
        Arc::new(S060ConfiguredCountingFactory {
            placement: "workspace",
            validate_calls: Arc::clone(&validate_calls),
            create_calls: Arc::clone(&factory_calls),
        }),
    );
    supervisor.set_root_agent_id(parent_id.clone());
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    let (sender, receiver) = tokio::sync::mpsc::channel(1_024);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    let tool = Arc::new(SpawnAgentTool {
        sender: sender.clone(),
        allowed_placements: vec!["workspace".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: parent_id.clone(),
        parent_budget: Arc::clone(&parent_budget),
        guidance: None,
    });
    drop(sender);

    let mut calls = tokio::task::JoinSet::new();
    for index in 0..1_000_u32 {
        let tool = Arc::clone(&tool);
        let capability = capability.clone();
        calls.spawn(async move {
            // Each marker is exactly 32 lowercase hexadecimal digits, so it
            // could fit the full suffix of a superficially valid child id.
            // Non-hex fixture prose could never appear there and would not
            // prove that ids are independent from caller-controlled text.
            let instructions = format!("a{index:031x}");
            let task = format!("b{index:031x}");
            let result = tool
                .call(
                    serde_json::json!({
                    "placement": "workspace",
                    "instructions": &instructions,
                    "task": &task,
                    "budget": s060_budget(1, 1, "0", 0)
                    }),
                    &capability,
                )
                .await;
            (result, instructions, task)
        });
    }

    let mut ids = std::collections::BTreeSet::new();
    let mut fixtures = Vec::new();
    while let Some(joined) = calls.join_next().await {
        let (result, instructions, task) = joined.expect("spawn task should not panic");
        let result = result.expect("authorized concurrent spawn should be accepted");
        let id = result["child_id"].as_str().expect("child id").to_string();
        // S061: hex-digit task text now surfaces (by design) as the
        // `/forge/<slug>` path segment; instructions must never leak.
        let slug = id
            .strip_prefix("/forge/")
            .unwrap_or_else(|| panic!("child id should be /forge-prefixed: {id}"));
        assert_eq!(slug.len(), 32);
        assert!(
            slug.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "child id slug must use only the task-text charset: {id}"
        );
        assert_eq!(slug, task, "slug must derive from this call's own task text");
        fixtures.push(instructions);
        assert!(
            ids.insert(id),
            "concurrent child ids must be pairwise unique"
        );
    }
    assert_eq!(ids.len(), 1_000);
    for id in &ids {
        assert!(!id.contains("workspace"));
        for fixture in &fixtures {
            assert!(
                !id.contains(fixture),
                "child id leaked instructions fixture {fixture}: {id}"
            );
        }
    }
    assert_eq!(
        parent_budget.lock().expect("root budget").used_sub_agents,
        1_000,
        "all accepted calls must reserve against one root-session budget"
    );
    assert_eq!(factory_calls.load(Ordering::SeqCst), 1_000);
    assert_eq!(validate_calls.load(Ordering::SeqCst), 1_000);
    drop(tool);
    actor.await.expect("single supervisor actor should stop");

    let entries = journal
        .read_all(&parent_id)
        .expect("one root-session journal should remain readable after child drain");
    assert_eq!(entries.len(), 2_000);
    assert!(entries.iter().all(|entry| entry.schema_version == 3));
    assert!(entries.iter().all(|entry| entry.agent_id == parent_id));

    let spawned_ids = entries
        .iter()
        .filter_map(|entry| match &entry.entry {
            JournalEntryKind::SubAgentSpawned {
                child_id,
                placement,
                backend,
                task,
                instructions,
            } => {
                assert_eq!(placement, "workspace");
                assert_eq!(backend, "native");
                assert!(task.starts_with('b'));
                assert!(instructions.as_deref().is_some_and(|value| value.starts_with('a')));
                Some(child_id.0.clone())
            }
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let completed_ids = entries
        .iter()
        .filter_map(|entry| match &entry.entry {
            JournalEntryKind::SubAgentCompleted { child_id, success } => {
                assert!(*success);
                Some(child_id.0.clone())
            }
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(spawned_ids.len(), 1_000);
    assert_eq!(completed_ids.len(), 1_000);
    assert_eq!(spawned_ids, ids);
    assert_eq!(completed_ids, ids);
}

#[tokio::test]
async fn s060_a33_real_supervisor_produces_typed_placement_lifecycle_events() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut supervisor = AgentSupervisor::with_task_factory(
        s060_capability(&["workspace"]),
        ResourceBudget::new(100, 4, Decimal::ZERO, 1),
        Arc::new(NoopFactory),
    );
    install_spawn_test_journal(&mut supervisor);
    supervisor.set_root_agent_id(AgentId("parent-s060-activity".into()));
    supervisor.set_activity_sink(Arc::new(S060LifecycleSink(Arc::clone(&events))));
    supervisor
        .spawn_agent(spawn_config_with_placement(
            "child-0123456789abcdef0123456789abcdef",
            "parent-s060-activity",
            "workspace",
            ResourceBudget::new(10, 1, Decimal::ZERO, 1),
        ))
        .expect("authorized placement should spawn");

    for _ in 0..100 {
        if events.lock().expect("activity event lock").len() >= 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    let serialized = events
        .lock()
        .expect("activity event lock")
        .iter()
        .map(|event| serde_json::to_value(event).expect("typed activity event should serialize"))
        .collect::<Vec<_>>();
    let spawned = serialized
        .iter()
        .find(|event| event["type"] == "ChildSpawned")
        .expect("real spawn producer should emit ChildSpawned");
    assert_eq!(spawned["placement"], "workspace");
    assert_eq!(spawned["task"], "delegate task");
    assert!(spawned.get("agent_type").is_none());
    let finished = serialized
        .iter()
        .find(|event| event["type"] == "ChildFinished")
        .expect("real completion producer should emit ChildFinished");
    assert_eq!(finished["placement"], "workspace");
    assert_eq!(finished["exit_reason"], "completed");
    assert!(finished.get("agent_type").is_none());
}
