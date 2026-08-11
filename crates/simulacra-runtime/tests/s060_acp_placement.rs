#![cfg(feature = "spawn")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_decimal::Decimal;
use simulacra_config::{SimulacraConfig, build_capability_token};
use simulacra_runtime::{
    AcpChildFuture, AcpChildRequest, AcpChildRuntime, ActivitySink, AgentInputQueue,
    AgentLoopOutput, AgentSupervisor, AgentTaskFactory, CancelChildAgentTool, CancellationToken,
    InMemoryJournalStorage, ProviderKind, SpawnAgentTool, SteerChildAgentTool,
};
use simulacra_types::{
    ActivityEvent, AgentId, CapabilityToken, ExitReason, FsMetadata, JournalEntryKind,
    JournalStorage, Message, ResourceBudget, Role, TokenUsage, Tool, VfsError, VfsSnapshot,
    VirtualFs,
};

const PLACEMENT: &str = "workspace";
const PROFILE: &str = "workspace-pod";

#[derive(Debug)]
struct CapturedAcpCall {
    request: AcpChildRequest,
}

#[derive(Debug, PartialEq, Eq)]
struct LiveControlObservation {
    steering: String,
    cancellation_observed: bool,
}

struct CapturingAcpRuntime {
    calls: Arc<AtomicUsize>,
    capture: Mutex<Option<tokio::sync::oneshot::Sender<CapturedAcpCall>>>,
    live_controls: Mutex<Option<tokio::sync::oneshot::Sender<LiveControlObservation>>>,
}

impl CapturingAcpRuntime {
    fn new(
        observe_live_controls: bool,
    ) -> (
        Arc<Self>,
        tokio::sync::oneshot::Receiver<CapturedAcpCall>,
        Option<tokio::sync::oneshot::Receiver<LiveControlObservation>>,
        Arc<AtomicUsize>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (controls_tx, controls_rx) = tokio::sync::oneshot::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                calls: Arc::clone(&calls),
                capture: Mutex::new(Some(tx)),
                live_controls: Mutex::new(observe_live_controls.then_some(controls_tx)),
            }),
            rx,
            observe_live_controls.then_some(controls_rx),
            calls,
        )
    }
}

impl AcpChildRuntime for CapturingAcpRuntime {
    fn start_child(
        &self,
        request: AcpChildRequest,
        cancellation: CancellationToken,
        activity_sink: Arc<dyn ActivitySink>,
        input_queue: AgentInputQueue,
    ) -> AcpChildFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let captured = CapturedAcpCall { request };
        if let Some(tx) = self.capture.lock().expect("capture lock poisoned").take() {
            let _ = tx.send(captured);
        }

        let live_controls = self
            .live_controls
            .lock()
            .expect("live-control lock poisoned")
            .take();
        if let Some(live_controls) = live_controls {
            return Box::pin(async move {
                let mut input_queue = input_queue;
                let steering = input_queue.recv().await.ok_or_else(|| {
                    simulacra_runtime::RuntimeError::Session(
                        "supervisor closed the live ACP input queue".into(),
                    )
                })?;
                activity_sink.emit(ActivityEvent::ToolOutput {
                    tool_call_id: "s060-live-sink".into(),
                    line: "ACP runtime observed live steering".into(),
                });
                while !cancellation.is_cancelled() {
                    tokio::task::yield_now().await;
                }
                let _ = live_controls.send(LiveControlObservation {
                    steering,
                    cancellation_observed: true,
                });
                Ok(AgentLoopOutput {
                    exit_reason: ExitReason::Cancelled,
                    messages: vec![],
                    token_usage: TokenUsage::default(),
                    reported_tool_uses: None,
                    used_turns: 0,
                    used_cost: Decimal::ZERO,
                })
            });
        }

        Box::pin(async {
            Ok(AgentLoopOutput {
                exit_reason: ExitReason::Complete,
                messages: vec![Message {
                    role: Role::Assistant,
                    content: "ACP child completed".into(),
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_content: vec![],
                }],
                token_usage: TokenUsage::default(),
                reported_tool_uses: None,
                used_turns: 1,
                used_cost: Decimal::ZERO,
            })
        })
    }
}

#[derive(Default)]
struct NativeConstructionProbe {
    vfs_accesses: AtomicUsize,
    cell_configurations: AtomicUsize,
    tool_registrations: AtomicUsize,
    provider_constructions: AtomicUsize,
}

struct PanicNativeFs {
    probe: Arc<NativeConstructionProbe>,
}

impl PanicNativeFs {
    fn native_access(&self) -> ! {
        self.probe.vfs_accesses.fetch_add(1, Ordering::SeqCst);
        panic!("ACP placement must not construct or inspect a native VFS")
    }
}

impl VirtualFs for PanicNativeFs {
    fn read(&self, _path: &str) -> Result<Vec<u8>, VfsError> {
        self.native_access()
    }

    fn write(&self, _path: &str, _data: &[u8]) -> Result<(), VfsError> {
        self.native_access()
    }

    fn exists(&self, _path: &str) -> bool {
        self.native_access()
    }

    fn list_dir(&self, _path: &str) -> Result<Vec<String>, VfsError> {
        self.native_access()
    }

    fn mkdir(&self, _path: &str) -> Result<(), VfsError> {
        self.native_access()
    }

    fn remove(&self, _path: &str) -> Result<(), VfsError> {
        self.native_access()
    }

    fn metadata(&self, _path: &str) -> Result<FsMetadata, VfsError> {
        self.native_access()
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.native_access()
    }

    fn restore(&self, _snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.native_access()
    }
}

#[derive(Default)]
struct RecordingActivitySink {
    events: Mutex<Vec<ActivityEvent>>,
}

impl ActivitySink for RecordingActivitySink {
    fn emit(&self, event: ActivityEvent) {
        self.events
            .lock()
            .expect("activity lock poisoned")
            .push(event);
    }
}

struct AcpHarness {
    tool: SpawnAgentTool,
    capture: Option<tokio::sync::oneshot::Receiver<CapturedAcpCall>>,
    live_controls: Option<tokio::sync::oneshot::Receiver<LiveControlObservation>>,
    runtime_calls: Arc<AtomicUsize>,
    native_probe: Arc<NativeConstructionProbe>,
    journal: Arc<InMemoryJournalStorage>,
    parent_activity: Arc<RecordingActivitySink>,
    parent_budget: Arc<Mutex<ResourceBudget>>,
    supervisor_sender: tokio::sync::mpsc::Sender<simulacra_runtime::SupervisorMessage>,
    supervisor_task: tokio::task::JoinHandle<()>,
}

fn s060_config() -> SimulacraConfig {
    let source = r#"
[project]
name = "s060-acp-red"

[agent_types.root]
model = "parent-model"
allowed_child_placements = ["workspace"]

[child_placements.workspace]
backend = "acp"
acp_profile = "workspace-pod"
"#;
    let config: SimulacraConfig = toml::from_str(source).expect("RED fixture should parse");
    config.validate().expect("RED fixture should validate");
    config
}

fn parent_capability_for_red(
    config: &SimulacraConfig,
    authorized_placements: &[&str],
) -> CapabilityToken {
    let mut capability = build_capability_token(
        config
            .agent_types
            .get("root")
            .expect("root agent type should exist"),
    );
    capability.spawn_placements = authorized_placements
        .iter()
        .map(|placement| (*placement).to_string())
        .collect();
    capability
}

fn acp_harness(
    inject_runtime: bool,
    observe_live_controls: bool,
    authorized_placements: &[&str],
) -> AcpHarness {
    let config = s060_config();
    let parent_capability = parent_capability_for_red(&config, authorized_placements);
    let parent_budget = Arc::new(Mutex::new(ResourceBudget::new(
        10_000,
        20,
        Decimal::ZERO,
        4,
    )));
    let native_probe = Arc::new(NativeConstructionProbe::default());
    let journal = Arc::new(InMemoryJournalStorage::new());
    let parent_activity = Arc::new(RecordingActivitySink::default());
    let (runtime, capture, live_controls, runtime_calls) =
        CapturingAcpRuntime::new(observe_live_controls);

    let cell_probe = Arc::clone(&native_probe);
    let tools_probe = Arc::clone(&native_probe);
    let provider_probe = Arc::clone(&native_probe);
    let factory = Arc::new(AgentTaskFactory {
        config,
        provider_kind: ProviderKind::Anthropic,
        vfs: Arc::new(PanicNativeFs {
            probe: Arc::clone(&native_probe),
        }),
        journal: Arc::clone(&journal) as Arc<dyn JournalStorage>,
        activity_sink: Arc::clone(&parent_activity) as Arc<dyn ActivitySink>,
        parent_capability: parent_capability.clone(),
        allowed_mcp_servers: None,
        supervisor_sender: None,
        pipeline: None,
        script_executor: None,
        child_cell_configurator: Some(Arc::new(move |_cell| {
            cell_probe
                .cell_configurations
                .fetch_add(1, Ordering::SeqCst);
        })),
        child_tool_registrar: Some(Arc::new(move |_registry, _cell| {
            tools_probe
                .tool_registrations
                .fetch_add(1, Ordering::SeqCst);
            Ok(())
        })),
        child_provider_factory: Some(Arc::new(move |_kind, _model| {
            provider_probe
                .provider_constructions
                .fetch_add(1, Ordering::SeqCst);
            panic!("ACP placement must not construct a native provider")
        })),
        acp_child_runtime: inject_runtime.then_some(runtime),
    });

    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        parent_capability.clone(),
        Arc::clone(&parent_budget),
        factory,
    );
    supervisor.set_root_agent_id(AgentId("parent-root".into()));
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let supervisor_task = tokio::spawn(async move {
        supervisor.run_actor_loop(receiver).await;
    });
    let tool = spawn_tool_for_red(
        sender,
        authorized_placements,
        Arc::clone(&parent_activity),
        Arc::clone(&parent_budget),
    );
    let supervisor_sender = tool.sender.clone();

    AcpHarness {
        tool,
        capture: Some(capture),
        live_controls,
        runtime_calls,
        native_probe,
        journal,
        parent_activity,
        parent_budget,
        supervisor_sender,
        supervisor_task,
    }
}

fn spawn_tool_for_red(
    sender: tokio::sync::mpsc::Sender<simulacra_runtime::SupervisorMessage>,
    authorized_placements: &[&str],
    parent_activity: Arc<RecordingActivitySink>,
    parent_budget: Arc<Mutex<ResourceBudget>>,
) -> SpawnAgentTool {
    SpawnAgentTool {
        sender,
        allowed_placements: authorized_placements
            .iter()
            .map(|placement| (*placement).to_string())
            .collect(),
        activity_sink: parent_activity,
        parent_id: AgentId("parent-root".into()),
        parent_budget,
        guidance: None,
    }
}

fn spawn_arguments(instructions: Option<&str>) -> serde_json::Value {
    let mut value = serde_json::json!({
        "placement": PLACEMENT,
        "task": "  preserve this delegated task  ",
        "budget": {
            "max_tokens": 321,
            "max_turns": 7,
            "max_cost": "0",
            "max_sub_agents": 1
        }
    });
    if let Some(instructions) = instructions {
        value
            .as_object_mut()
            .expect("spawn arguments should be an object")
            .insert("instructions".into(), instructions.into());
    }
    value
}

async fn finish_harness(harness: AcpHarness) {
    drop(harness.supervisor_sender);
    drop(harness.tool);
    tokio::time::timeout(Duration::from_secs(1), harness.supervisor_task)
        .await
        .expect("supervisor should stop after its senders are dropped")
        .expect("supervisor task should not panic");
}

async fn captured_call(harness: &mut AcpHarness) -> CapturedAcpCall {
    let capture = harness
        .capture
        .take()
        .expect("ACP capture receiver should only be consumed once");
    tokio::time::timeout(Duration::from_secs(1), capture)
        .await
        .expect("ACP runtime should be called before the timeout")
        .expect("ACP runtime should publish its captured request")
}

fn assert_no_native_construction(probe: &NativeConstructionProbe) {
    assert_eq!(probe.vfs_accesses.load(Ordering::SeqCst), 0);
    assert_eq!(probe.cell_configurations.load(Ordering::SeqCst), 0);
    assert_eq!(probe.tool_registrations.load(Ordering::SeqCst), 0);
    assert_eq!(probe.provider_constructions.load(Ordering::SeqCst), 0);
}

fn parent_spawn_entry_count(journal: &InMemoryJournalStorage) -> usize {
    journal
        .read_all(&AgentId("parent-root".into()))
        .expect("parent journal read should succeed")
        .iter()
        .filter(|entry| matches!(entry.entry, JournalEntryKind::SubAgentSpawned { .. }))
        .count()
}

#[tokio::test]
async fn a21_acp_placement_delivers_distinct_byte_identical_instructions_and_task() {
    let mut harness = acp_harness(true, false, &[PLACEMENT]);
    let instructions = "  use the evidence skill\nthen report exactly  ";

    let acknowledgement = harness
        .tool
        .call(
            spawn_arguments(Some(instructions)),
            &parent_capability_for_red(&s060_config(), &[PLACEMENT]),
        )
        .await
        .expect("an ACP placement with instructions should be accepted");
    assert_eq!(acknowledgement["status"], "running");

    let captured = captured_call(&mut harness).await;
    assert_eq!(captured.request.task, "  preserve this delegated task  ");
    assert_eq!(captured.request.instructions.as_deref(), Some(instructions));
    assert_eq!(harness.runtime_calls.load(Ordering::SeqCst), 1);
    assert_eq!(captured.request.placement, PLACEMENT);
    finish_harness(harness).await;
}

#[tokio::test]
async fn a22_acp_placement_without_instructions_is_task_only() {
    let mut harness = acp_harness(true, false, &[PLACEMENT]);

    harness
        .tool
        .call(
            spawn_arguments(None),
            &parent_capability_for_red(&s060_config(), &[PLACEMENT]),
        )
        .await
        .expect("an ACP placement without instructions should be accepted");
    let captured = captured_call(&mut harness).await;
    assert_eq!(captured.request.task, "  preserve this delegated task  ");
    assert_eq!(captured.request.instructions, None);
    finish_harness(harness).await;
}

#[tokio::test]
async fn a23_live_cancellation_activity_and_input_controls_are_separate_arguments() {
    let mut harness = acp_harness(true, true, &[PLACEMENT]);

    let acknowledgement = harness
        .tool
        .call(
            spawn_arguments(Some("shape independently")),
            &parent_capability_for_red(&s060_config(), &[PLACEMENT]),
        )
        .await
        .expect("ACP placement should be accepted");
    let child_id = acknowledgement["child_id"]
        .as_str()
        .expect("accepted spawn should return a child id")
        .to_string();
    let _typed_request = captured_call(&mut harness).await.request;

    let steer = SteerChildAgentTool {
        sender: harness.supervisor_sender.clone(),
        caller_id: AgentId("parent-root".into()),
    };
    let steering = "  preserve this live steering message  ";
    steer
        .call(
            serde_json::json!({"child_id": child_id, "message": steering}),
            &CapabilityToken::default(),
        )
        .await
        .expect("supervisor should deliver steering to the live ACP input queue");

    let cancel = CancelChildAgentTool {
        sender: harness.supervisor_sender.clone(),
        caller_id: AgentId("parent-root".into()),
    };
    cancel
        .call(
            serde_json::json!({"child_id": child_id}),
            &CapabilityToken::default(),
        )
        .await
        .expect("supervisor should signal the live ACP cancellation token");

    let controls = tokio::time::timeout(
        Duration::from_secs(1),
        harness
            .live_controls
            .take()
            .expect("live-control receiver should be installed"),
    )
    .await
    .expect("ACP runtime should observe live controls before timeout")
    .expect("ACP runtime should report its live-control observations");
    assert_eq!(
        controls,
        LiveControlObservation {
            steering: steering.to_string(),
            cancellation_observed: true,
        }
    );
    let activity_json = serde_json::to_value(
        harness
            .parent_activity
            .events
            .lock()
            .expect("activity lock poisoned")
            .clone(),
    )
    .expect("recorded activity should serialize");
    assert!(
        activity_json
            .to_string()
            .contains("ACP runtime observed live steering"),
        "the activity sink passed separately to ACP must remain connected to the parent: {activity_json}"
    );
    drop(steer);
    drop(cancel);
    finish_harness(harness).await;
}

#[tokio::test]
async fn a23_acp_request_serde_has_exactly_the_eight_normative_keys() {
    let mut harness = acp_harness(true, false, &[PLACEMENT]);
    let instructions = "  typed ACP shaping instructions  ";
    harness
        .tool
        .call(
            spawn_arguments(Some(instructions)),
            &parent_capability_for_red(&s060_config(), &[PLACEMENT]),
        )
        .await
        .expect("ACP placement should produce a typed request");
    let captured = captured_call(&mut harness).await;
    let request_json = serde_json::to_value(&captured.request)
        .expect("AcpChildRequest should serialize as its public contract");
    let expected_budget = ResourceBudget::new(321, 7, Decimal::ZERO, 1);
    let expected_capability = CapabilityToken::default();

    assert_eq!(
        request_json,
        serde_json::json!({
            "child_id": captured.request.child_id.0,
            "parent_id": "parent-root",
            "placement": PLACEMENT,
            "acp_profile": PROFILE,
            "instructions": instructions,
            "task": "  preserve this delegated task  ",
            "budget": serde_json::to_value(&expected_budget).expect("expected budget JSON"),
            "capability": serde_json::to_value(&expected_capability)
                .expect("expected capability JSON")
        })
    );
    for live_control in [
        "cancellation",
        "cancellation_token",
        "activity_sink",
        "input",
        "input_queue",
    ] {
        assert!(
            request_json.get(live_control).is_none(),
            "live control {live_control:?} must remain a separate start_child argument"
        );
    }

    let round_trip: AcpChildRequest = serde_json::from_value(request_json)
        .expect("the exact AcpChildRequest JSON should deserialize");
    assert_eq!(round_trip.child_id.0, captured.request.child_id.0);
    assert_eq!(round_trip.parent_id.0, "parent-root");
    assert_eq!(round_trip.placement, PLACEMENT);
    assert_eq!(round_trip.acp_profile, PROFILE);
    assert_eq!(round_trip.instructions.as_deref(), Some(instructions));
    assert_eq!(round_trip.task, "  preserve this delegated task  ");
    assert_eq!(
        serde_json::to_value(&round_trip.budget).expect("round-trip budget JSON"),
        serde_json::to_value(&expected_budget).expect("expected budget JSON")
    );
    assert_eq!(
        serde_json::to_value(&round_trip.capability).expect("round-trip capability JSON"),
        serde_json::to_value(&expected_capability).expect("expected capability JSON")
    );
    finish_harness(harness).await;
}

#[tokio::test]
async fn a24_acp_placement_bypasses_every_native_child_environment_constructor() {
    let mut harness = acp_harness(true, false, &[PLACEMENT]);

    harness
        .tool
        .call(
            spawn_arguments(Some("work inside the supplied placement")),
            &parent_capability_for_red(&s060_config(), &[PLACEMENT]),
        )
        .await
        .expect("ACP placement should be accepted without native construction");
    let captured = captured_call(&mut harness).await;

    assert_eq!(harness.runtime_calls.load(Ordering::SeqCst), 1);
    assert_no_native_construction(&harness.native_probe);
    assert_eq!(captured.request.placement, PLACEMENT);
    finish_harness(harness).await;
}

#[tokio::test]
async fn a25_unknown_placement_fails_before_running_acknowledgement() {
    const MISSING: &str = "missing-workspace";
    let unknown_harness = acp_harness(true, false, &[PLACEMENT, MISSING]);
    let mut unknown_arguments = spawn_arguments(None);
    unknown_arguments["placement"] = MISSING.into();
    let budget_before = unknown_harness
        .parent_budget
        .lock()
        .expect("budget lock poisoned")
        .clone();
    let unknown_error = unknown_harness
        .tool
        .call(
            unknown_arguments,
            &parent_capability_for_red(&s060_config(), &[PLACEMENT, MISSING]),
        )
        .await
        .expect_err("unknown placement must fail synchronously");
    let unknown_message = unknown_error.to_string();
    assert!(unknown_message.contains("placement"), "{unknown_message}");
    assert!(unknown_message.contains(MISSING), "{unknown_message}");
    assert!(unknown_message.contains(PLACEMENT), "{unknown_message}");
    assert_eq!(unknown_harness.runtime_calls.load(Ordering::SeqCst), 0);
    assert_no_native_construction(&unknown_harness.native_probe);
    assert_eq!(
        parent_spawn_entry_count(&unknown_harness.journal),
        0,
        "unknown placement must not append SubAgentSpawned"
    );
    assert_eq!(
        unknown_harness
            .journal
            .read_all(&AgentId("parent-root".into()))
            .expect("journal read should succeed")
            .len(),
        0,
        "unknown placement must not journal an accepted spawn"
    );
    let budget_after = unknown_harness
        .parent_budget
        .lock()
        .expect("budget lock poisoned")
        .clone();
    assert_eq!(budget_after.used_sub_agents, budget_before.used_sub_agents);
    finish_harness(unknown_harness).await;
}

#[tokio::test]
async fn a25_missing_acp_runtime_fails_before_running_acknowledgement() {
    let missing_runtime_harness = acp_harness(false, false, &[PLACEMENT]);
    let budget_before = missing_runtime_harness
        .parent_budget
        .lock()
        .expect("budget lock poisoned")
        .clone();
    let missing_runtime_error = missing_runtime_harness
        .tool
        .call(
            spawn_arguments(None),
            &parent_capability_for_red(&s060_config(), &[PLACEMENT]),
        )
        .await
        .expect_err("missing ACP runtime must fail before a running acknowledgement");
    let missing_runtime_message = missing_runtime_error.to_string();
    assert!(
        missing_runtime_message.contains("ACP"),
        "{missing_runtime_message}"
    );
    assert!(
        missing_runtime_message.contains("placement"),
        "{missing_runtime_message}"
    );
    assert!(
        !missing_runtime_message.contains("agent_type"),
        "{missing_runtime_message}"
    );
    assert!(
        missing_runtime_message.contains(PLACEMENT),
        "{missing_runtime_message}"
    );
    assert!(
        missing_runtime_message.contains(PROFILE),
        "{missing_runtime_message}"
    );
    assert_eq!(
        missing_runtime_harness.runtime_calls.load(Ordering::SeqCst),
        0
    );
    assert_no_native_construction(&missing_runtime_harness.native_probe);
    assert_eq!(
        parent_spawn_entry_count(&missing_runtime_harness.journal),
        0,
        "missing ACP runtime must not append SubAgentSpawned"
    );
    let budget_after = missing_runtime_harness
        .parent_budget
        .lock()
        .expect("budget lock poisoned")
        .clone();
    assert_eq!(budget_after.used_sub_agents, budget_before.used_sub_agents);
    finish_harness(missing_runtime_harness).await;
}
