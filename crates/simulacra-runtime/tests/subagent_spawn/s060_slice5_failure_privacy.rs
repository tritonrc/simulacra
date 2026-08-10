const S060_PROVIDER_ERROR_SECRET: &str = "SECRET-S060-PROVIDER-ERROR-9C41";
const S060_PROVIDER_TASK_SECRET: &str = "SECRET-S060-PROVIDER-TASK-43A7";
const S060_PROVIDER_INSTRUCTIONS_SECRET: &str = "SECRET-S060-PROVIDER-INSTRUCTIONS-508D";
const S060_PROVIDER_SKILL_SECRET: &str = "SECRET-S060-PROVIDER-SKILL-B15E";

const S060_ACP_ERROR_SECRET: &str = "SECRET-S060-ACP-ERROR-267F";
const S060_ACP_TASK_SECRET: &str = "SECRET-S060-ACP-TASK-C691";
const S060_ACP_INSTRUCTIONS_SECRET: &str = "SECRET-S060-ACP-INSTRUCTIONS-E204";
const S060_ACP_SKILL_SECRET: &str = "SECRET-S060-ACP-SKILL-176B";

const S060_SKILL_ERROR_SECRET: &str = "SECRET-S060-MISSING-SKILL-75DC";
const S060_SKILL_TASK_SECRET: &str = "SECRET-S060-SKILL-TASK-10F4";
const S060_SKILL_INSTRUCTIONS_SECRET: &str = "SECRET-S060-SKILL-INSTRUCTIONS-A38C";

const S060_RUNTIME_ERROR_SECRET: &str = "SECRET-S060-RUNTIME-ERROR-F7A0";
const S060_RUNTIME_TASK_SECRET: &str = "SECRET-S060-RUNTIME-TASK-942E";
const S060_RUNTIME_INSTRUCTIONS_SECRET: &str = "SECRET-S060-RUNTIME-INSTRUCTIONS-6B31";

#[derive(Default)]
struct S060PrivacyGate {
    started: Notify,
    release: Notify,
}

struct S060PrivacyGatedFactory {
    inner: Arc<dyn TaskFactory>,
    gate: Arc<S060PrivacyGate>,
}

impl TaskFactory for S060PrivacyGatedFactory {
    fn validate_spawn_config(&self, config: &SpawnConfig) -> Result<(), RuntimeError> {
        self.inner.validate_spawn_config(config)
    }

    fn placement_backend(&self, config: &SpawnConfig) -> AgentBackend {
        self.inner.placement_backend(config)
    }

    fn prepare_spawn_config(&self, config: &mut SpawnConfig) -> Result<(), RuntimeError> {
        self.inner.prepare_spawn_config(config)
    }

    fn prepare_spawn_config_for_caller(
        &self,
        config: &mut SpawnConfig,
        caller_capability: &CapabilityToken,
    ) -> Result<(), RuntimeError> {
        self.inner
            .prepare_spawn_config_for_caller(config, caller_capability)
    }

    fn after_spawn(
        &self,
        config: &SpawnConfig,
        result: &simulacra_runtime::SpawnResult,
    ) -> Result<(), RuntimeError> {
        self.inner.after_spawn(config, result)
    }

    fn create_task(&self, config: SpawnConfig, cancellation: CancellationToken) -> BoxTaskFuture {
        let task = self.inner.create_task(config, cancellation);
        let gate = Arc::clone(&self.gate);
        Box::pin(async move {
            gate.started.notify_one();
            gate.release.notified().await;
            task.await
        })
    }

    fn create_task_with_input_and_budget(
        &self,
        config: SpawnConfig,
        cancellation: CancellationToken,
        input_queue: simulacra_runtime::AgentInputQueue,
        budget: Arc<Mutex<ResourceBudget>>,
    ) -> BoxTaskFuture {
        let task =
            self.inner
                .create_task_with_input_and_budget(config, cancellation, input_queue, budget);
        let gate = Arc::clone(&self.gate);
        Box::pin(async move {
            gate.started.notify_one();
            gate.release.notified().await;
            task.await
        })
    }
}

struct S060PrivacyChatFailureProvider {
    calls: Arc<AtomicUsize>,
}

impl Provider for S060PrivacyChatFailureProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(ProviderError::Other(S060_PROVIDER_ERROR_SECRET.to_string())) })
    }
}

struct S060PrivacyFailingAcpRuntime {
    calls: Arc<AtomicUsize>,
}

impl simulacra_runtime::AcpChildRuntime for S060PrivacyFailingAcpRuntime {
    fn start_child(
        &self,
        _request: simulacra_runtime::AcpChildRequest,
        _cancellation: CancellationToken,
        _activity_sink: Arc<dyn simulacra_runtime::ActivitySink>,
        _input_queue: simulacra_runtime::AgentInputQueue,
    ) -> simulacra_runtime::AcpChildFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(RuntimeError::Session(S060_ACP_ERROR_SECRET.to_string())) })
    }
}

struct S060PrivacyRuntimeFactory {
    calls: Arc<AtomicUsize>,
}

impl TaskFactory for S060PrivacyRuntimeFactory {
    fn validate_spawn_config(&self, _config: &SpawnConfig) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn create_task(&self, _config: SpawnConfig, _token: CancellationToken) -> BoxTaskFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(RuntimeError::Session(S060_RUNTIME_ERROR_SECRET.to_string())) })
    }
}

fn s060_async_capture_storage() -> &'static (
    Arc<Mutex<Vec<CapturedSpan>>>,
    Arc<Mutex<Vec<CapturedEvent>>>,
) {
    static STORAGE: OnceLock<(
        Arc<Mutex<Vec<CapturedSpan>>>,
        Arc<Mutex<Vec<CapturedEvent>>>,
    )> = OnceLock::new();
    STORAGE.get_or_init(|| {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
            spans: Arc::clone(&spans),
            events: Arc::clone(&events),
        });
        tracing::subscriber::set_global_default(subscriber)
            .expect("S060 async telemetry tests require the integration-test global subscriber");
        (spans, events)
    })
}

async fn s060_capture_trace_async<T>(
    operation: impl FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = T>>>,
) -> (T, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    static ASYNC_CAPTURE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _capture_guard = ASYNC_CAPTURE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (spans, events) = s060_async_capture_storage();
    spans
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    tracing::callsite::rebuild_interest_cache();
    let result = operation().await;
    let captured_spans = spans
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let captured_events = events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    (result, captured_spans, captured_events)
}

fn s060_privacy_capability(placement: &str, skill: Option<&str>) -> CapabilityToken {
    CapabilityToken {
        spawn_placements: vec![placement.to_string()],
        paths_read: vec![PathPattern("/skills/**".into())],
        skill_patterns: skill
            .into_iter()
            .map(|name| format!("skill:{name}"))
            .collect(),
        ..CapabilityToken::default()
    }
}

fn s060_privacy_native_config(placement: &str, skill: &str) -> SimulacraConfig {
    s060_parse_runtime_config(&format!(
        r#"
[project]
name = "s060-native-privacy"

[agent_types.root]
model = "root-model"
allowed_child_placements = ["{placement}"]

[child_placements.{placement}]
backend = "native"
model = "child-model"
skills = ["{skill}"]

[child_placements.{placement}.capabilities]
paths_read = ["/skills/**"]
skill_patterns = ["skill:{skill}"]
"#,
    ))
}

fn s060_privacy_acp_config(placement: &str) -> SimulacraConfig {
    s060_parse_runtime_config(&format!(
        r#"
[project]
name = "s060-acp-privacy"

[agent_types.root]
model = "root-model"
allowed_child_placements = ["{placement}"]

[child_placements.{placement}]
backend = "acp"
acp_profile = "opaque-failure-profile"
skills = ["{S060_ACP_SKILL_SECRET}"]

[child_placements.{placement}.capabilities]
skill_patterns = ["skill:{S060_ACP_SKILL_SECRET}"]
"#,
    ))
}

fn s060_privacy_factory(
    config: SimulacraConfig,
    capability: CapabilityToken,
    vfs: Arc<dyn VirtualFs>,
    journal: Arc<InMemoryJournalStorage>,
    child_provider_factory: Option<simulacra_runtime::ChildProviderFactory>,
    acp_child_runtime: Option<Arc<dyn simulacra_runtime::AcpChildRuntime>>,
) -> AgentTaskFactory {
    AgentTaskFactory {
        config,
        provider_kind: ProviderKind::Anthropic,
        vfs,
        journal: Arc::clone(&journal) as Arc<dyn JournalStorage>,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: capability,
        allowed_mcp_servers: None,
        supervisor_sender: None,
        parent_model: "root-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory,
        acp_child_runtime,
    }
}

struct S060PrivacyRun {
    terminal: serde_json::Value,
    spans: Vec<CapturedSpan>,
    events: Vec<CapturedEvent>,
}

async fn s060_run_accepted_failure(
    placement: &'static str,
    task: &'static str,
    instructions: &'static str,
    capability: CapabilityToken,
    factory: Arc<dyn TaskFactory>,
    journal: Arc<InMemoryJournalStorage>,
) -> S060PrivacyRun {
    let gate = Arc::new(S060PrivacyGate::default());
    let gated_factory: Arc<dyn TaskFactory> = Arc::new(S060PrivacyGatedFactory {
        inner: factory,
        gate: Arc::clone(&gate),
    });
    let parent_id = AgentId(format!("root-s060-privacy-{placement}"));

    let ((ack, running, terminal), spans, events) = s060_capture_trace_async(|| {
        let capability = capability.clone();
        let journal = Arc::clone(&journal);
        let gate = Arc::clone(&gate);
        let parent_id = parent_id.clone();
        let gated_factory = Arc::clone(&gated_factory);
        Box::pin(async move {
            let shared_budget = Arc::new(Mutex::new(ResourceBudget::new(
                1_024,
                16,
                Decimal::new(10, 0),
                1,
            )));
            let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
                capability.clone(),
                Arc::clone(&shared_budget),
                gated_factory,
            );
            supervisor.set_root_agent_id(parent_id.clone());
            supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
            let (sender, receiver) = tokio::sync::mpsc::channel(8);
            let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
            let spawn_tool = SpawnAgentTool {
                sender: sender.clone(),
                allowed_placements: vec![placement.to_string()],
                activity_sink: Arc::new(NoopActivitySink),
                parent_id: parent_id.clone(),
                parent_budget: Arc::clone(&shared_budget),
                guidance: None,
            };
            let status_tool = ChildStatusTool {
                sender: sender.clone(),
                caller_id: parent_id.clone(),
            };
            let join_tool = JoinChildAgentTool {
                sender: sender.clone(),
                caller_id: parent_id,
            };
            let ack = spawn_tool
                .call(
                    serde_json::json!({
                        "placement": placement,
                        "instructions": instructions,
                        "task": task,
                        "budget": {
                            "max_tokens": 64,
                            "max_turns": 2,
                            "max_cost": "1",
                            "max_sub_agents": 1
                        }
                    }),
                    &capability,
                )
                .await
                .expect("the real SpawnAgentTool must return an accepted running handle");
            assert_eq!(ack["status"], "running");
            let child_id = ack["child_id"]
                .as_str()
                .expect("running acknowledgement must contain the generated child id")
                .to_string();

            tokio::time::timeout(Duration::from_secs(2), gate.started.notified())
                .await
                .expect("accepted child task must reach its execution gate");
            let running = status_tool
                .call(serde_json::json!({"child_id": child_id}), &capability)
                .await
                .expect("accepted child must be queryable before release");
            assert_eq!(running["status"], "running");
            assert_eq!(running["ready"], false);

            gate.release.notify_one();
            let terminal = join_tool
                .call(serde_json::json!({"child_id": child_id}), &capability)
                .await
                .expect("join must return the real asynchronous terminal failure");
            assert_eq!(terminal["status"], "failed");
            assert_eq!(terminal["ready"], true);

            drop(spawn_tool);
            drop(status_tool);
            drop(join_tool);
            drop(sender);
            actor.await.expect("supervisor actor must stop cleanly");
            (ack, running, terminal)
        })
    })
    .await;

    assert_eq!(ack["placement"], placement);
    assert_eq!(running["placement"], placement);
    S060PrivacyRun {
        terminal,
        spans,
        events,
    }
}

fn s060_assert_bounded_failure_telemetry(
    run: &S060PrivacyRun,
    expected_category: &str,
    forbidden: &[&str],
) {
    let warnings = run
        .events
        .iter()
        .filter(|event| {
            event.level == "WARN"
                && event.fields.get("child_id").map(String::as_str)
                    == run.terminal["child_id"].as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        warnings.len(),
        1,
        "one accepted child failure must emit exactly one WARN; warnings={warnings:?}, all events={:?}",
        run.events
    );
    let failure = warnings[0];
    let category = failure
        .fields
        .get("error_category")
        .map(String::as_str)
        .expect("failure WARN must carry the bounded error_category field");
    assert!(
        ["provider", "acp_runtime", "runtime"].contains(&category),
        "error_category must use S060's bounded vocabulary, got {category:?}"
    );
    assert_eq!(
        category, expected_category,
        "the bounded category must preserve the native-provider/ACP-port/runtime mapping"
    );
    let actual_fields = failure
        .fields
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let allowed_fields = [
        "message",
        "child_id",
        "parent_id",
        "placement",
        "error_category",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    assert_eq!(
        actual_fields, allowed_fields,
        "failure WARN must contain only bounded identity/category fields"
    );

    let encoded = format!("{:?}{:?}", run.spans, run.events);
    for secret in forbidden {
        assert!(
            !encoded.contains(secret),
            "captured tracing events/spans leaked {secret}: {encoded}"
        );
    }
    assert!(
        run.spans
            .iter()
            .all(|span| !span.fields.contains_key("error_category")),
        "error_category is a log field only, not a span/metric dimension: {:?}",
        run.spans
    );
}

#[tokio::test(flavor = "current_thread")]
async fn s060_a41_real_native_provider_chat_failure_is_bounded_after_running_ack() {
    let placement = "native_provider_failure";
    let capability = s060_privacy_capability(placement, Some(S060_PROVIDER_SKILL_SECRET));
    let journal = Arc::new(InMemoryJournalStorage::new());
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/skills").expect("skills root");
    vfs.mkdir(&format!("/skills/{S060_PROVIDER_SKILL_SECRET}"))
        .expect("configured skill directory");
    vfs.write(
        &format!("/skills/{S060_PROVIDER_SKILL_SECRET}/SKILL.md"),
        format!(
            "---\nname: {S060_PROVIDER_SKILL_SECRET}\ndescription: bounded fixture\n---\nfixture body\n"
        )
        .as_bytes(),
    )
    .expect("configured skill document");
    let provider_constructions = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let provider_constructions_for_factory = Arc::clone(&provider_constructions);
    let provider_calls_for_factory = Arc::clone(&provider_calls);
    let factory = s060_privacy_factory(
        s060_privacy_native_config(placement, S060_PROVIDER_SKILL_SECRET),
        capability.clone(),
        vfs,
        Arc::clone(&journal),
        Some(Arc::new(move |_kind, _model| {
            provider_constructions_for_factory.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(S060PrivacyChatFailureProvider {
                calls: Arc::clone(&provider_calls_for_factory),
            }))
        })),
        None,
    );

    let run = s060_run_accepted_failure(
        placement,
        S060_PROVIDER_TASK_SECRET,
        S060_PROVIDER_INSTRUCTIONS_SECRET,
        capability,
        Arc::new(factory),
        journal,
    )
    .await;

    assert_eq!(provider_constructions.load(Ordering::SeqCst), 1);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert!(
        run.terminal["message"]
            .as_str()
            .is_some_and(|message| message.contains(S060_PROVIDER_ERROR_SECRET)),
        "terminal result must prove the real Provider::chat error occurred"
    );
    s060_assert_bounded_failure_telemetry(
        &run,
        "provider",
        &[
            S060_PROVIDER_ERROR_SECRET,
            S060_PROVIDER_TASK_SECRET,
            S060_PROVIDER_INSTRUCTIONS_SECRET,
            S060_PROVIDER_SKILL_SECRET,
        ],
    );
}

#[tokio::test(flavor = "current_thread")]
async fn s060_a41_real_acp_port_failure_is_bounded_after_running_ack() {
    let placement = "acp_port_failure";
    let capability = s060_privacy_capability(placement, Some(S060_ACP_SKILL_SECRET));
    let journal = Arc::new(InMemoryJournalStorage::new());
    let acp_calls = Arc::new(AtomicUsize::new(0));
    let factory = s060_privacy_factory(
        s060_privacy_acp_config(placement),
        capability.clone(),
        Arc::new(MemoryFs::new()),
        Arc::clone(&journal),
        None,
        Some(Arc::new(S060PrivacyFailingAcpRuntime {
            calls: Arc::clone(&acp_calls),
        })),
    );

    let run = s060_run_accepted_failure(
        placement,
        S060_ACP_TASK_SECRET,
        S060_ACP_INSTRUCTIONS_SECRET,
        capability,
        Arc::new(factory),
        journal,
    )
    .await;

    assert_eq!(acp_calls.load(Ordering::SeqCst), 1);
    assert!(
        run.terminal["message"]
            .as_str()
            .is_some_and(|message| message.contains(S060_ACP_ERROR_SECRET)),
        "terminal result must prove the real ACP-port error occurred"
    );
    s060_assert_bounded_failure_telemetry(
        &run,
        "acp_runtime",
        &[
            S060_ACP_ERROR_SECRET,
            S060_ACP_TASK_SECRET,
            S060_ACP_INSTRUCTIONS_SECRET,
            S060_ACP_SKILL_SECRET,
        ],
    );
}

#[tokio::test(flavor = "current_thread")]
async fn s060_a41_real_native_skill_discovery_failure_hides_configured_skill_name() {
    let placement = "native_skill_failure";
    let capability = s060_privacy_capability(placement, Some(S060_SKILL_ERROR_SECRET));
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider_constructions = Arc::new(AtomicUsize::new(0));
    let provider_constructions_for_factory = Arc::clone(&provider_constructions);
    let factory = s060_privacy_factory(
        s060_privacy_native_config(placement, S060_SKILL_ERROR_SECRET),
        capability.clone(),
        Arc::new(MemoryFs::new()),
        Arc::clone(&journal),
        Some(Arc::new(move |_kind, _model| {
            provider_constructions_for_factory.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeProvider::new(Vec::new())))
        })),
        None,
    );

    let run = s060_run_accepted_failure(
        placement,
        S060_SKILL_TASK_SECRET,
        S060_SKILL_INSTRUCTIONS_SECRET,
        capability,
        Arc::new(factory),
        journal,
    )
    .await;

    assert_eq!(
        provider_constructions.load(Ordering::SeqCst),
        1,
        "the native provider must construct before real child-environment skill discovery"
    );
    assert!(
        run.terminal["message"]
            .as_str()
            .is_some_and(|message| message.contains(S060_SKILL_ERROR_SECRET)),
        "terminal result must prove configured-skill discovery failed"
    );
    s060_assert_bounded_failure_telemetry(
        &run,
        "runtime",
        &[
            S060_SKILL_ERROR_SECRET,
            S060_SKILL_TASK_SECRET,
            S060_SKILL_INSTRUCTIONS_SECRET,
        ],
    );
}

#[tokio::test(flavor = "current_thread")]
async fn s060_a41_non_provider_runtime_failure_is_bounded_after_running_ack() {
    let placement = "other_runtime_failure";
    let capability = s060_privacy_capability(placement, None);
    let journal = Arc::new(InMemoryJournalStorage::new());
    let runtime_calls = Arc::new(AtomicUsize::new(0));
    let factory: Arc<dyn TaskFactory> = Arc::new(S060PrivacyRuntimeFactory {
        calls: Arc::clone(&runtime_calls),
    });

    let run = s060_run_accepted_failure(
        placement,
        S060_RUNTIME_TASK_SECRET,
        S060_RUNTIME_INSTRUCTIONS_SECRET,
        capability,
        factory,
        journal,
    )
    .await;

    assert_eq!(runtime_calls.load(Ordering::SeqCst), 1);
    assert!(
        run.terminal["message"]
            .as_str()
            .is_some_and(|message| message.contains(S060_RUNTIME_ERROR_SECRET)),
        "terminal result must prove the non-provider runtime error occurred"
    );
    s060_assert_bounded_failure_telemetry(
        &run,
        "runtime",
        &[
            S060_RUNTIME_ERROR_SECRET,
            S060_RUNTIME_TASK_SECRET,
            S060_RUNTIME_INSTRUCTIONS_SECRET,
        ],
    );
}
