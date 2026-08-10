fn s060_slice2_spawn_arguments(placement: &str) -> serde_json::Value {
    serde_json::json!({
        "placement": placement,
        "instructions": "bounded native worker",
        "task": "same bounded work",
        "budget": s060_budget(128, 3, "1", 1)
    })
}

#[test]
fn s060_native_placement_missing_skill_error_uses_placement_vocabulary() {
    let config = s060_parse_runtime_config(
        r#"
[project]
name = "s060-native-missing-skill"

[agent_types.root]
model = "root-model"
allowed_child_placements = ["workspace"]

[child_placements.workspace]
backend = "native"
model = "child-model"
skills = ["missing-native-skill"]

[child_placements.workspace.capabilities]
skill_patterns = ["skill:missing-native-skill"]
"#,
    );
    let mut parent_capability = s060_capability(&["workspace"]);
    parent_capability.skill_patterns = vec!["skill:missing-native-skill".into()];
    let mut factory = s060_real_task_factory(config);
    factory.parent_capability = parent_capability;
    factory.child_provider_factory = Some(Arc::new(|_kind, _model| {
        Ok(Box::new(FakeProvider::new(Vec::new())))
    }));

    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime builds")
        .block_on(factory.create_task(
            s060_supervisor_request(
                "workspace",
                ResourceBudget::new(10, 1, Decimal::new(1, 0), 1),
            ),
            CancellationToken::new(Duration::from_secs(1)),
        ))
        .expect_err("an undiscoverable configured native skill must reject child construction")
        .to_string();

    assert!(
        error.contains("placement"),
        "error should name the placement concept: {error}"
    );
    assert!(
        error.contains("workspace"),
        "error should name the configured placement: {error}"
    );
    assert!(
        error.contains("missing-native-skill"),
        "error should name the missing skill: {error}"
    );
    assert!(
        !error.to_lowercase().contains("agent type"),
        "child placement failures must not use removed role vocabulary: {error}"
    );
}

// S060 A29: contend through real SpawnAgentTool calls and the actor-owned
// supervisor boundary. The in-memory journal is the accepted-spawn audit
// surface; activity events are not used as a proxy for durable journal state.
#[tokio::test]
async fn s060_a29_parallel_tool_calls_reserve_exactly_one_child() {
    let parent_id = AgentId("parent-s060-native-contention".into());
    let capability = s060_capability(&["in_process"]);
    let parent_budget = ResourceBudget::new(4_096, 128, Decimal::new(32, 0), 1);
    let (tool, receiver, budget) =
        s060_spawn_tool_with_budget_handle(&["in_process"], parent_budget, None);
    let journal = Arc::new(InMemoryJournalStorage::new());
    let factory = Arc::new(
        RecordingTaskFactory::new(vec![Ok(child_success_output())]).with_journal_capture(
            Arc::clone(&journal) as Arc<dyn JournalStorage>,
            parent_id.clone(),
        ),
    );
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&budget),
        Arc::clone(&factory) as Arc<dyn TaskFactory>,
    );
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    supervisor.set_root_agent_id(parent_id.clone());
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    let tool = Arc::new(SpawnAgentTool {
        parent_id: parent_id.clone(),
        ..tool
    });

    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let tool = Arc::clone(&tool);
        let capability = capability.clone();
        calls.spawn(async move {
            tool.call(s060_slice2_spawn_arguments("in_process"), &capability)
                .await
        });
    }

    let mut acknowledgements = Vec::new();
    let mut reservation_failures = Vec::new();
    while let Some(result) = calls.join_next().await {
        match result.expect("spawn call task should not panic") {
            Ok(ack) => acknowledgements.push(ack),
            Err(error) => reservation_failures.push(error.to_string()),
        }
    }

    assert_eq!(
        acknowledgements.len(),
        1,
        "exactly one call may be accepted"
    );
    assert_eq!(
        acknowledgements[0]["status"], "running",
        "the sole successful call must receive the real supervisor acknowledgement"
    );
    assert_eq!(reservation_failures.len(), 31);
    assert!(
        reservation_failures
            .iter()
            .all(|error| error.contains("sub_agents") || error.contains("sub-agent")),
        "all losing calls must fail the reservation, got {reservation_failures:?}"
    );
    assert_eq!(
        factory.started_count(),
        1,
        "only one task may be constructed"
    );
    assert_eq!(
        budget.lock().expect("budget lock").used_sub_agents,
        1,
        "reservation must be atomic at the supervisor boundary"
    );
    let spawned = journal
        .read_all(&parent_id)
        .expect("parent journal should be readable")
        .into_iter()
        .filter(|entry| matches!(entry.entry, JournalEntryKind::SubAgentSpawned { .. }))
        .count();
    assert_eq!(spawned, 1, "only the accepted child may be journaled");

    drop(tool);
    actor.await.expect("supervisor actor should stop cleanly");
}

struct S060Slice2CountingHook(Arc<AtomicUsize>);

impl simulacra_hooks::HookModule for S060Slice2CountingHook {
    fn name(&self) -> &str {
        "slice2-counting-hook"
    }

    fn invoke(
        &self,
        _phase: simulacra_hooks::Phase,
        operation: simulacra_hooks::Operation,
        _context: &str,
    ) -> Result<simulacra_hooks::Verdict, simulacra_hooks::HookError> {
        assert_eq!(operation, simulacra_hooks::Operation::Spawn);
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(simulacra_hooks::Verdict::Continue(None))
    }
}

fn s060_a30_config() -> SimulacraConfig {
    let config: SimulacraConfig = toml::from_str(
        r#"
[project]
name = "s060-a30-invalid-placement"

[agent_types.default]
model = "parent-model"
allowed_child_placements = ["workspace"]

[child_placements.workspace]
backend = "native"
model = "child-model"

[child_placements.workspace.capabilities]
shell = true
"#,
    )
    .expect("A30 root and child placement config should parse");
    config
        .validate()
        .expect("A30 root and child placement config should validate");
    config
}

// S060 A30: concurrent invalid requests are rejected at the tool boundary,
// before the actual hook pipeline, supervisor reservation, journal, ACP/native
// runtime factory, or acknowledgement can observe an accepted child.
#[tokio::test]
async fn s060_a30_concurrent_invalid_placements_have_zero_accepted_effects() {
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060Slice2CountingHook(Arc::clone(&hook_calls))),
    );
    let capability = s060_capability(&["workspace"]);
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider_constructions = Arc::new(AtomicUsize::new(0));
    let provider_constructions_for_factory = Arc::clone(&provider_constructions);
    let factory = Arc::new(AgentTaskFactory {
        config: s060_a30_config(),
        provider_kind: ProviderKind::Anthropic,
        vfs: Arc::new(MemoryFs::new()),
        journal: Arc::clone(&journal) as Arc<dyn JournalStorage>,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: capability.clone(),
        allowed_mcp_servers: None,
        supervisor_sender: None,
        pipeline: Some(Arc::new(pipeline)),
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: Some(Arc::new(move |_kind, _model| {
            provider_constructions_for_factory.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeProvider::new(Vec::new())))
        })),
        acp_child_runtime: None,
    });
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        1_024,
        16,
        Decimal::new(8, 0),
        1,
    )));
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&budget),
        factory,
    );
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    let tool = SpawnAgentTool {
        sender: sender.clone(),
        allowed_placements: vec!["workspace".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-s060-native-a30".into()),
        parent_budget: Arc::clone(&budget),
        guidance: None,
    };
    let tool = Arc::new(tool);
    let mut calls = tokio::task::JoinSet::new();
    for index in 0..32 {
        let tool = Arc::clone(&tool);
        let capability = capability.clone();
        calls.spawn(async move {
            let placement = if index % 2 == 0 { "" } else { "unknown" };
            (
                placement,
                tool.call(s060_slice2_spawn_arguments(placement), &capability)
                    .await,
            )
        });
    }

    let mut denials = 0;
    while let Some(result) = calls.join_next().await {
        let (placement, result) = result.expect("invalid spawn call should not panic");
        let error = result.expect_err("empty and unknown placements must be denied");
        let message = error.to_string();
        assert!(
            message.contains("placement"),
            "denial should name the rejected placement field: {message}"
        );
        if placement == "unknown" {
            assert!(message.contains("unknown"));
            assert!(
                message.contains("workspace"),
                "unknown-placement denial should list the available placement: {message}"
            );
        }
        denials += 1;
    }
    assert_eq!(denials, 32);
    assert_eq!(budget.lock().expect("budget lock").used_sub_agents, 0);
    assert!(
        journal
            .read_all(&AgentId("parent-s060-native-a30".into()))
            .expect("journal read")
            .is_empty()
    );
    assert_eq!(hook_calls.load(Ordering::SeqCst), 0);
    assert_eq!(provider_constructions.load(Ordering::SeqCst), 0);

    drop(tool);
    drop(sender);
    actor.await.expect("supervisor actor should stop cleanly");
}

// S060 A30: the redundant host allow-list and caller capability checks are
// independently deny-all. Holding the receiver proves neither call dispatched
// a supervisor message (and therefore neither could receive an acknowledgement).
#[tokio::test]
async fn s060_a30_empty_tool_and_token_lists_each_deny_all() {
    let finite_parent = || ResourceBudget::new(1_024, 16, Decimal::new(8, 0), 1);

    let (tool_denies, mut tool_receiver) = s060_spawn_tool(&[], finite_parent(), None);
    let caller_allows = s060_capability(&["workspace"]);
    let tool_error = tool_denies
        .call(s060_slice2_spawn_arguments("workspace"), &caller_allows)
        .await
        .expect_err("empty tool placement list must deny every call");
    assert!(tool_error.to_string().contains("workspace"));
    assert!(
        tool_receiver.try_recv().is_err(),
        "denied tool call must not dispatch"
    );

    let (tool_allows, mut token_receiver) = s060_spawn_tool(&["workspace"], finite_parent(), None);
    let caller_denies = s060_capability(&[]);
    let token_error = tool_allows
        .call(s060_slice2_spawn_arguments("workspace"), &caller_denies)
        .await
        .expect_err("empty caller spawn placement token must deny every call");
    assert!(token_error.to_string().contains("workspace"));
    assert!(
        token_receiver.try_recv().is_err(),
        "denied token call must not dispatch"
    );
}

#[test]
fn s060_agent_task_factory_rejects_native_placement_with_missing_model() {
    let mut config = s060_a30_config();
    config
        .child_placements
        .get_mut("workspace")
        .expect("workspace placement should exist")
        .model = None;
    let factory = s060_real_task_factory(config);

    let error = factory
        .validate_spawn_config(&s060_supervisor_request(
            "workspace",
            ResourceBudget::new(10, 1, Decimal::ZERO, 0),
        ))
        .expect_err("malformed native placement must be rejected before defaulting to an empty model");
    let message = error.to_string();
    assert!(
        message.contains("workspace") && message.contains("model"),
        "malformed placement error should name the placement and missing model: {message}"
    );
}

#[test]
fn s060_agent_task_factory_never_coerces_blank_or_unicode_unknown_placements_to_native() {
    let factory = s060_real_task_factory(s060_a30_config());
    for placement in ["", " \t\n", "\u{2003}", "workspace\u{2003}", "未知"] {
        let error = factory
            .validate_spawn_config(&s060_supervisor_request(
                placement,
                ResourceBudget::new(10, 1, Decimal::ZERO, 0),
            ))
            .expect_err("non-configured placement must be denied rather than defaulting native");
        assert!(
            error.to_string().contains("placement"),
            "denial should identify placement for {placement:?}: {error}"
        );
    }
}
