use super::*;
use crate::{
    AgentSupervisor, ChannelActivitySink, InMemoryJournalStorage, NoopActivitySink,
    RestartStrategy, TaskFactory,
};
use std::sync::atomic::{AtomicBool, Ordering};

use simulacra_types::{
    ActivityEvent, ExitReason, FsMetadata, JournalEntryKind, MemoryCapability, MemoryPath,
    PathPattern, Role, TokenUsage, Tool, VfsError, VfsSnapshot,
};
use simulacra_vfs::MemoryFs;

const STOCK_SPAWN_AGENT_DESCRIPTION: &str = "I can start a supervised child for one concrete, bounded, independent task. Choose where I run it with placement and shape how it works with instructions; placement supplies an environment and capabilities, not a role. I return a live handle, not the child's final answer.";

fn spawn_tool_with_guidance(
    guidance: Option<SpawnAgentGuidance>,
) -> (
    SpawnAgentTool,
    tokio::sync::mpsc::Receiver<SupervisorMessage>,
) {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    (
        SpawnAgentTool {
            sender,
            allowed_placements: vec!["reviewer".into()],
            activity_sink: Arc::new(NoopActivitySink),
            parent_id: AgentId("parent-agent".into()),
            parent_budget: Arc::new(Mutex::new(ResourceBudget::new(100, 10, Decimal::ZERO, 1))),
            guidance,
        },
        receiver,
    )
}

async fn call_spawn_tool_with_ack(
    tool: &SpawnAgentTool,
    mut receiver: tokio::sync::mpsc::Receiver<SupervisorMessage>,
) -> serde_json::Value {
    let arguments = serde_json::json!({
        "placement": "reviewer",
        "task": "review the focused change",
        "budget": {
            "max_tokens": 10,
            "max_turns": 1,
            "max_cost": "0",
            "max_sub_agents": 1
        }
    });
    let capability = CapabilityToken {
        spawn_placements: vec!["reviewer".into()],
        ..Default::default()
    };
    let call = tool.call(arguments, &capability);
    let acknowledge = async move {
        let message = receiver
            .recv()
            .await
            .expect("spawn tool should send a supervisor request");
        match message.payload {
            SupervisorPayload::Spawn(_, result_tx) => result_tx
                .send(Ok(crate::supervisor::SpawnAck {
                    child_id: AgentId("child-fixed".into()),
                    placement: "reviewer".into(),
                    backend: AgentBackend::Native,
                }))
                .expect("spawn tool should await the acknowledgement"),
            other => panic!("expected spawn request, got {other:?}"),
        }
    };
    let (result, ()) = tokio::join!(call, acknowledge);
    result.expect("spawn should return its acknowledgement")
}

#[tokio::test]
async fn absent_spawn_guidance_preserves_stock_definition_and_acknowledgement_bytes() {
    let (tool, receiver) = spawn_tool_with_guidance(None);

    assert_eq!(tool.definition().description, STOCK_SPAWN_AGENT_DESCRIPTION);
    let acknowledgement = call_spawn_tool_with_ack(&tool, receiver).await;
    assert_eq!(
        serde_json::to_vec(&acknowledgement).expect("acknowledgement should encode"),
        br#"{"child_id":"child-fixed","placement":"reviewer","status":"running"}"#
    );
    assert!(acknowledgement.get("note").is_none());
}

#[tokio::test]
async fn spawn_guidance_overrides_description_and_appends_verbatim_result_note() {
    let description = "Host lifecycle guidance.\nWait for a wake before harvesting.";
    let result_note = "Child accepted; keep working until the host wakes you.\nDo not poll.";
    let (tool, receiver) = spawn_tool_with_guidance(Some(SpawnAgentGuidance {
        description: description.into(),
        result_note: Some(result_note.into()),
    }));

    assert_eq!(tool.definition().description, description);
    let acknowledgement = call_spawn_tool_with_ack(&tool, receiver).await;
    assert_eq!(
        acknowledgement.get("note").and_then(|note| note.as_str()),
        Some(result_note)
    );
}

#[tokio::test]
async fn spawn_guidance_with_authored_empty_result_note_keeps_present_note_key() {
    let (tool, receiver) = spawn_tool_with_guidance(Some(SpawnAgentGuidance {
        description: "Host lifecycle guidance.".into(),
        result_note: Some(String::new()),
    }));

    let acknowledgement = call_spawn_tool_with_ack(&tool, receiver).await;
    assert_eq!(acknowledgement.get("note"), Some(&serde_json::json!("")));
}

#[tokio::test]
async fn spawn_guidance_without_result_note_overrides_description_and_omits_note() {
    let description = "Use the host-managed child lifecycle verbatim.";
    let (tool, receiver) = spawn_tool_with_guidance(Some(SpawnAgentGuidance {
        description: description.into(),
        result_note: None,
    }));

    assert_eq!(tool.definition().description, description);
    let acknowledgement = call_spawn_tool_with_ack(&tool, receiver).await;
    assert!(acknowledgement.get("note").is_none());
}

fn parent_with_memory() -> CapabilityToken {
    CapabilityToken {
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![PathPattern("/workspace/**".into())],
        memory: MemoryCapability {
            enabled: true,
            search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
            write_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
        },
        ..Default::default()
    }
}

#[test]
fn override_without_memory_inherits_parent_memory() {
    // W1 regression: when the spawn_agent capabilities override has no
    // memory field, intersecting parent ∩ override must NOT strip the
    // parent's memory grants. The helper inherits parent.memory into
    // the override before intersect.
    let parent = parent_with_memory();
    let override_no_memory = CapabilityToken {
        // Match parent exactly so the path intersection has something to keep —
        // the focus of this test is the memory dimension, not path intersection.
        paths_read: vec![PathPattern("/**".into())],
        ..Default::default()
    };
    let with_memory = inherit_memory_when_override_unset(&override_no_memory, &parent);
    let intersected = parent.intersect(&with_memory);

    assert!(
        intersected.memory.enabled,
        "child must inherit parent memory when override doesn't author memory"
    );
    assert_eq!(
        intersected
            .memory
            .search_scopes
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>(),
        vec!["/var/memory/self"]
    );
}

#[test]
fn override_authoring_memory_is_not_overwritten() {
    // If a future override does author memory (e.g. narrows scopes),
    // the helper must NOT clobber it with parent.memory.
    let parent = parent_with_memory();
    let override_narrower = CapabilityToken {
        memory: MemoryCapability {
            enabled: true,
            search_scopes: vec![MemoryPath::parse("/var/memory/self/notes").unwrap()],
            write_scopes: vec![],
        },
        ..Default::default()
    };
    let merged = inherit_memory_when_override_unset(&override_narrower, &parent);
    // Should be the override's value, not parent's.
    assert_eq!(
        merged.memory.search_scopes[0].as_str(),
        "/var/memory/self/notes",
        "helper must not overwrite an override that authored memory"
    );
    assert!(merged.memory.write_scopes.is_empty());
}

#[test]
fn override_with_disabled_default_memory_inherits_parent() {
    // The override carries MemoryCapability::default() (disabled, empty)
    // because parse_capability_override has no JSON path for memory.
    // The helper must inherit parent memory in this case.
    let parent = parent_with_memory();
    let override_default = CapabilityToken::default();
    let merged = inherit_memory_when_override_unset(&override_default, &parent);
    assert!(merged.memory.enabled);
    assert_eq!(merged.memory.search_scopes.len(), 1);
}

#[test]
fn parent_without_memory_means_child_inherits_disabled() {
    // If parent has no memory, the child must also have no memory.
    let parent = CapabilityToken::default();
    let override_default = CapabilityToken::default();
    let merged = inherit_memory_when_override_unset(&override_default, &parent);
    assert!(!merged.memory.enabled);
}

#[test]
fn child_proc_runtime_overlays_child_proc_state_and_delegates_mailbox() {
    let inherited = Arc::new(MemoryFs::new());
    inherited.mkdir("/proc").unwrap();
    inherited.mkdir("/proc/mailbox").unwrap();
    inherited
        .write("/proc/mailbox/report.md", b"report")
        .unwrap();
    let inherited_vfs: Arc<dyn VirtualFs> = inherited;
    let inherited_journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let mut capability = CapabilityToken {
        javascript: true,
        ..Default::default()
    };
    capability.paths_read = vec![PathPattern("/**".into())];
    let runtime = child_proc_runtime(
        inherited_vfs,
        inherited_journal,
        ChildProcSpec {
            agent_id: AgentId("child-1".into()),
            agent_name: "researcher".into(),
            model: "child-model".into(),
            parent_id: AgentId("parent-1".into()),
            capability,
            budget: ResourceBudget::new(100, 4, Decimal::ZERO, 0),
            pipeline: None,
        },
    );
    runtime.tools.set(vec![ToolDefinition {
        name: "file_read".into(),
        description: "read".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }]);

    assert_eq!(runtime.vfs.read("/proc/agent/id").unwrap(), b"child-1");
    assert_eq!(runtime.vfs.read("/proc/agent/name").unwrap(), b"researcher");
    assert_eq!(
        runtime.vfs.read("/proc/agent/parent_id").unwrap(),
        b"parent-1"
    );
    assert_eq!(
        runtime.vfs.read("/proc/capabilities/javascript").unwrap(),
        b"true"
    );
    assert_eq!(
        runtime.vfs.read("/proc/mailbox/report.md").unwrap(),
        b"report",
        "child-specific ProcFs must still delegate mailbox paths to the inherited stack"
    );
    assert_eq!(
        runtime.vfs.list_dir("/proc/tools").unwrap(),
        vec!["file_read"]
    );
}

type ScriptedAcpHandler = dyn Fn(
        AcpChildRequest,
        CancellationToken,
        Arc<dyn ActivitySink>,
        AgentInputQueue,
    ) -> crate::AcpChildFuture
    + Send
    + Sync;

struct ScriptedAcpRuntime {
    handler: Arc<ScriptedAcpHandler>,
}

impl ScriptedAcpRuntime {
    fn new<F>(handler: F) -> Arc<Self>
    where
        F: Fn(
                AcpChildRequest,
                CancellationToken,
                Arc<dyn ActivitySink>,
                AgentInputQueue,
            ) -> crate::AcpChildFuture
            + Send
            + Sync
            + 'static,
    {
        Arc::new(Self {
            handler: Arc::new(handler),
        })
    }
}

impl AcpChildRuntime for ScriptedAcpRuntime {
    fn start_child(
        &self,
        request: AcpChildRequest,
        cancellation: CancellationToken,
        activity_sink: Arc<dyn ActivitySink>,
        input_queue: AgentInputQueue,
    ) -> crate::AcpChildFuture {
        (self.handler)(request, cancellation, activity_sink, input_queue)
    }
}

struct PanicFs;

impl VirtualFs for PanicFs {
    fn read(&self, _path: &str) -> Result<Vec<u8>, VfsError> {
        panic!("ACP children must not read through the native VFS")
    }

    fn write(&self, _path: &str, _data: &[u8]) -> Result<(), VfsError> {
        panic!("ACP children must not write through the native VFS")
    }

    fn exists(&self, _path: &str) -> bool {
        panic!("ACP children must not inspect the native VFS")
    }

    fn list_dir(&self, _path: &str) -> Result<Vec<String>, VfsError> {
        panic!("ACP children must not list the native VFS")
    }

    fn mkdir(&self, _path: &str) -> Result<(), VfsError> {
        panic!("ACP children must not mutate the native VFS")
    }

    fn remove(&self, _path: &str) -> Result<(), VfsError> {
        panic!("ACP children must not mutate the native VFS")
    }

    fn metadata(&self, _path: &str) -> Result<FsMetadata, VfsError> {
        panic!("ACP children must not inspect native VFS metadata")
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        panic!("ACP children must not snapshot the native VFS")
    }

    fn restore(&self, _snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        panic!("ACP children must not restore the native VFS")
    }
}

fn s056_acp_config() -> SimulacraConfig {
    let toml_str = r#"
[project]
name = "s056"

[agent_types.parent]
model = "parent-model"

[child_placements.reviewer]
backend = "acp"
acp_profile = "codex-local"

[child_placements.reviewer.capabilities]
shell = true
paths_read = ["/workspace/**"]
paths_write = ["/workspace/out/**"]
"#;
    let config: SimulacraConfig = toml::from_str(toml_str).expect("S056 config should parse");
    config.validate().expect("S056 config should validate");
    config
}

fn s056_parent_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        paths_read: vec![PathPattern("/workspace/**".into())],
        paths_write: vec![PathPattern("/workspace/out/**".into())],
        spawn_placements: vec!["reviewer".into()],
        ..Default::default()
    }
}

fn s056_spawn_config() -> SpawnConfig {
    SpawnConfig {
        agent_id: AgentId("child-acp-1".into()),
        parent_id: AgentId("parent-1".into()),
        capability: None,
        budget: ResourceBudget::new(321, 7, Decimal::ZERO, 1),
        restart_strategy: RestartStrategy::LetCrash,
        placement: "reviewer".into(),
        task: "review the patch".into(),
        instructions: None,
    }
}

fn s056_acp_output(
    exit_reason: ExitReason,
    content: &str,
    token_usage: TokenUsage,
    used_turns: u32,
) -> AgentLoopOutput {
    AgentLoopOutput {
        exit_reason,
        messages: vec![Message {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }],
        token_usage,
        reported_tool_uses: None,
        used_turns,
        used_cost: Decimal::ZERO,
    }
}

fn s056_factory(
    acp_child_runtime: Option<Arc<dyn AcpChildRuntime>>,
    activity_sink: Arc<dyn ActivitySink>,
    native_cell_built: Arc<AtomicBool>,
    native_tools_registered: Arc<AtomicBool>,
) -> AgentTaskFactory {
    AgentTaskFactory {
        config: s056_acp_config(),
        provider_kind: ProviderKind::Anthropic,
        vfs: Arc::new(PanicFs),
        journal: Arc::new(InMemoryJournalStorage::new()),
        activity_sink,
        parent_capability: s056_parent_capability(),
        allowed_mcp_servers: None,
        supervisor_sender: None,
        pipeline: None,
        script_executor: None,
        child_cell_configurator: Some(Arc::new(move |_cell| {
            native_cell_built.store(true, Ordering::SeqCst);
        })),
        child_tool_registrar: Some(Arc::new(move |_registry, _cell| {
            native_tools_registered.store(true, Ordering::SeqCst);
            Ok(())
        })),
        child_provider_factory: None,
        acp_child_runtime,
    }
}

#[tokio::test]
async fn live_acp_child_receives_supervisor_steer_message_through_runtime_queue() {
    let (runtime_started_tx, runtime_started_rx) = tokio::sync::oneshot::channel();
    let runtime_started_tx = Arc::new(Mutex::new(Some(runtime_started_tx)));
    let (delivered_tx, delivered_rx) = tokio::sync::oneshot::channel();
    let delivered_tx = Arc::new(Mutex::new(Some(delivered_tx)));
    let runtime = ScriptedAcpRuntime::new(
        move |_request, _cancellation, _activity_sink, mut input_queue| {
            let runtime_started_tx = Arc::clone(&runtime_started_tx);
            let delivered_tx = Arc::clone(&delivered_tx);
            Box::pin(async move {
                runtime_started_tx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("runtime should report that it is waiting for steer input")
                    .send(())
                    .expect("test should still wait for the runtime to start");
                let message = input_queue
                    .recv()
                    .await
                    .expect("live supervisor input handle should keep the queue open");
                delivered_tx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("runtime should deliver one steer message")
                    .send(message)
                    .expect("test should still wait for the delivered steer");
                Ok(s056_acp_output(
                    ExitReason::Complete,
                    "steer received",
                    TokenUsage::default(),
                    1,
                ))
            })
        },
    );
    let factory = Arc::new(s056_factory(
        Some(runtime),
        Arc::new(NoopActivitySink),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    ));
    let mut supervisor = AgentSupervisor::with_task_factory(
        s056_parent_capability(),
        ResourceBudget::new(10_000, 20, Decimal::ZERO, 4),
        factory,
    );
    supervisor.set_root_agent_id(AgentId("parent-1".into()));
    supervisor.set_journal_storage(Arc::new(InMemoryJournalStorage::new()));
    let (supervisor_tx, supervisor_rx) = tokio::sync::mpsc::channel(8);
    let supervisor_task = tokio::spawn(async move {
        supervisor.run_actor_loop(supervisor_rx).await;
    });

    let (spawn_result_tx, spawn_result_rx) = tokio::sync::oneshot::channel();
    supervisor_tx
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-1".into()),
            payload: SupervisorPayload::Spawn(Box::new(s056_spawn_config()), spawn_result_tx),
        })
        .await
        .expect("supervisor should accept ACP spawn request");
    let child_id = spawn_result_rx
        .await
        .expect("supervisor should reply to ACP spawn request")
        .expect("ACP spawn should be accepted")
        .child_id;
    tokio::time::timeout(Duration::from_secs(1), runtime_started_rx)
        .await
        .expect("scripted ACP runtime should start")
        .expect("runtime start signal should arrive");

    let (steer_result_tx, steer_result_rx) = tokio::sync::oneshot::channel();
    supervisor_tx
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-1".into()),
            payload: SupervisorPayload::SteerChild(
                child_id,
                "inspect the changed files".into(),
                steer_result_tx,
            ),
        })
        .await
        .expect("supervisor should accept steer request");
    steer_result_rx
        .await
        .expect("supervisor should reply to steer request")
        .expect("live ACP child should accept steering");

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), delivered_rx)
            .await
            .expect("runtime should receive a queued steer")
            .expect("runtime should send received steer to test"),
        "inspect the changed files"
    );

    drop(supervisor_tx);
    supervisor_task
        .await
        .expect("supervisor should exit after ACP child completes");
}

#[tokio::test]
async fn live_acp_child_receives_supervisor_steers_in_enqueue_order() {
    let (runtime_started_tx, runtime_started_rx) = tokio::sync::oneshot::channel();
    let runtime_started_tx = Arc::new(Mutex::new(Some(runtime_started_tx)));
    let (delivered_tx, delivered_rx) = tokio::sync::oneshot::channel();
    let delivered_tx = Arc::new(Mutex::new(Some(delivered_tx)));
    let runtime = ScriptedAcpRuntime::new(
        move |_request, _cancellation, _activity_sink, mut input_queue| {
            let runtime_started_tx = Arc::clone(&runtime_started_tx);
            let delivered_tx = Arc::clone(&delivered_tx);
            Box::pin(async move {
                runtime_started_tx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("runtime should report that it is waiting for steer input")
                    .send(())
                    .expect("test should still wait for the runtime to start");
                let first = input_queue
                    .recv()
                    .await
                    .expect("first steer should be delivered while child is live");
                let second = input_queue
                    .recv()
                    .await
                    .expect("second steer should be delivered while child is live");
                delivered_tx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("runtime should deliver received steers")
                    .send(vec![first, second])
                    .expect("test should still wait for delivered steers");
                Ok(s056_acp_output(
                    ExitReason::Complete,
                    "steers received",
                    TokenUsage::default(),
                    1,
                ))
            })
        },
    );
    let factory = Arc::new(s056_factory(
        Some(runtime),
        Arc::new(NoopActivitySink),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    ));
    let mut supervisor = AgentSupervisor::with_task_factory(
        s056_parent_capability(),
        ResourceBudget::new(10_000, 20, Decimal::ZERO, 4),
        factory,
    );
    supervisor.set_root_agent_id(AgentId("parent-1".into()));
    supervisor.set_journal_storage(Arc::new(InMemoryJournalStorage::new()));
    let (supervisor_tx, supervisor_rx) = tokio::sync::mpsc::channel(8);
    let supervisor_task = tokio::spawn(async move {
        supervisor.run_actor_loop(supervisor_rx).await;
    });

    let (spawn_result_tx, spawn_result_rx) = tokio::sync::oneshot::channel();
    supervisor_tx
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-1".into()),
            payload: SupervisorPayload::Spawn(Box::new(s056_spawn_config()), spawn_result_tx),
        })
        .await
        .expect("supervisor should accept ACP spawn request");
    let child_id = spawn_result_rx
        .await
        .expect("supervisor should reply to ACP spawn request")
        .expect("ACP spawn should be accepted")
        .child_id;
    tokio::time::timeout(Duration::from_secs(1), runtime_started_rx)
        .await
        .expect("scripted ACP runtime should start")
        .expect("runtime start signal should arrive");

    for message in ["first steer", "second steer"] {
        let (steer_result_tx, steer_result_rx) = tokio::sync::oneshot::channel();
        supervisor_tx
            .send(SupervisorMessage {
                priority: MessagePriority::Command,
                agent_id: AgentId("parent-1".into()),
                payload: SupervisorPayload::SteerChild(
                    child_id.clone(),
                    message.into(),
                    steer_result_tx,
                ),
            })
            .await
            .expect("supervisor should accept steer request");
        steer_result_rx
            .await
            .expect("supervisor should reply to steer request")
            .expect("live ACP child should accept steering");
    }

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), delivered_rx)
            .await
            .expect("runtime should receive queued steers")
            .expect("runtime should send received steers to test"),
        vec!["first steer", "second steer"]
    );

    drop(supervisor_tx);
    supervisor_task
        .await
        .expect("supervisor should exit after ACP child completes");
}

#[tokio::test]
async fn steer_is_still_accepted_after_cancellation_has_begun_while_child_lives() {
    let (runtime_started_tx, runtime_started_rx) = tokio::sync::oneshot::channel();
    let runtime_started_tx = Arc::new(Mutex::new(Some(runtime_started_tx)));
    let (delivered_tx, delivered_rx) = tokio::sync::oneshot::channel();
    let delivered_tx = Arc::new(Mutex::new(Some(delivered_tx)));
    let runtime = ScriptedAcpRuntime::new(
        move |_request, _cancellation, _activity_sink, mut input_queue| {
            let runtime_started_tx = Arc::clone(&runtime_started_tx);
            let delivered_tx = Arc::clone(&delivered_tx);
            Box::pin(async move {
                runtime_started_tx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("runtime should report that it is waiting for steer input")
                    .send(())
                    .expect("test should still wait for the runtime to start");
                let message = input_queue.recv().await.expect(
                    "live supervisor input handle should keep the queue open \
                     even after cancellation has begun, while the child task is still live",
                );
                delivered_tx
                    .lock()
                    .unwrap()
                    .take()
                    .expect("runtime should deliver the steer received after cancellation began")
                    .send(message)
                    .expect("test should still wait for the delivered steer");
                Ok(s056_acp_output(
                    ExitReason::Complete,
                    "steer received after cancellation began",
                    TokenUsage::default(),
                    1,
                ))
            })
        },
    );
    let factory = Arc::new(s056_factory(
        Some(runtime),
        Arc::new(NoopActivitySink),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    ));
    let mut supervisor = AgentSupervisor::with_task_factory(
        s056_parent_capability(),
        ResourceBudget::new(10_000, 20, Decimal::ZERO, 4),
        factory,
    );
    supervisor.set_root_agent_id(AgentId("parent-1".into()));
    supervisor.set_journal_storage(Arc::new(InMemoryJournalStorage::new()));
    let (supervisor_tx, supervisor_rx) = tokio::sync::mpsc::channel(8);
    let supervisor_task = tokio::spawn(async move {
        supervisor.run_actor_loop(supervisor_rx).await;
    });

    let (spawn_result_tx, spawn_result_rx) = tokio::sync::oneshot::channel();
    supervisor_tx
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-1".into()),
            payload: SupervisorPayload::Spawn(Box::new(s056_spawn_config()), spawn_result_tx),
        })
        .await
        .expect("supervisor should accept ACP spawn request");
    let child_id = spawn_result_rx
        .await
        .expect("supervisor should reply to ACP spawn request")
        .expect("ACP spawn should be accepted")
        .child_id;
    tokio::time::timeout(Duration::from_secs(1), runtime_started_rx)
        .await
        .expect("scripted ACP runtime should start")
        .expect("runtime start signal should arrive");

    // Cancellation begins, but the scripted runtime keeps running (it has not
    // observed the cancellation token yet) — the child task is still live.
    let (cancel_result_tx, cancel_result_rx) = tokio::sync::oneshot::channel();
    supervisor_tx
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-1".into()),
            payload: SupervisorPayload::CancelChild(child_id.clone(), cancel_result_tx),
        })
        .await
        .expect("supervisor should accept cancel request");
    cancel_result_rx
        .await
        .expect("supervisor should reply to cancel request")
        .expect("live ACP child should accept cancellation");

    let (steer_result_tx, steer_result_rx) = tokio::sync::oneshot::channel();
    supervisor_tx
        .send(SupervisorMessage {
            priority: MessagePriority::Command,
            agent_id: AgentId("parent-1".into()),
            payload: SupervisorPayload::SteerChild(
                child_id.clone(),
                "inspect the changed files despite cancellation".into(),
                steer_result_tx,
            ),
        })
        .await
        .expect("supervisor should accept steer request");
    steer_result_rx
        .await
        .expect("supervisor should reply to steer request")
        .expect("steer must still be accepted for a child whose cancellation has begun while it is still live");

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), delivered_rx)
            .await
            .expect("scripted runtime should actually receive the post-cancellation steer")
            .expect("runtime should send the received steer to test"),
        "inspect the changed files despite cancellation"
    );

    drop(supervisor_tx);
    supervisor_task
        .await
        .expect("supervisor should exit after ACP child completes");
}

#[tokio::test]
async fn s056_acp_factory_delegates_request_without_native_environment() {
    let requests = Arc::new(Mutex::new(Vec::<AcpChildRequest>::new()));
    let requests_for_runtime = Arc::clone(&requests);
    let runtime =
        ScriptedAcpRuntime::new(move |request, cancellation, activity_sink, _input_queue| {
            let requests = Arc::clone(&requests_for_runtime);
            Box::pin(async move {
                assert!(!cancellation.is_cancelled());
                activity_sink.emit(ActivityEvent::Token {
                    text: "delegated".into(),
                });
                requests.lock().unwrap().push(request);
                Ok(s056_acp_output(
                    ExitReason::Complete,
                    "ACP terminal summary",
                    TokenUsage {
                        input_tokens: 13,
                        output_tokens: 21,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    3,
                ))
            })
        });

    let native_cell_built = Arc::new(AtomicBool::new(false));
    let native_tools_registered = Arc::new(AtomicBool::new(false));
    let factory = s056_factory(
        Some(runtime),
        Arc::new(NoopActivitySink),
        Arc::clone(&native_cell_built),
        Arc::clone(&native_tools_registered),
    );

    let output = factory
        .create_task(
            s056_spawn_config(),
            CancellationToken::new(Duration::from_millis(50)),
        )
        .await
        .expect("ACP child should run through injected runtime");

    assert_eq!(
        output.messages.last().unwrap().content,
        "ACP terminal summary"
    );
    assert!(!native_cell_built.load(Ordering::SeqCst));
    assert!(!native_tools_registered.load(Ordering::SeqCst));

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.child_id, AgentId("child-acp-1".into()));
    assert_eq!(request.parent_id, AgentId("parent-1".into()));
    assert_eq!(request.placement, "reviewer");
    assert_eq!(request.acp_profile, "codex-local");
    assert!(
        request.instructions.is_none(),
        "absent ACP spawn instructions must remain None rather than becoming an empty prompt"
    );
    assert_eq!(request.task, "review the patch");
    assert_eq!(request.budget.max_tokens, 321);
    assert_eq!(request.budget.max_turns, 7);
    assert!(request.capability.shell);
    assert_eq!(
        request.capability.paths_read,
        vec![PathPattern("/workspace/**".into())]
    );
    assert_eq!(
        request.capability.paths_write,
        vec![PathPattern("/workspace/out/**".into())]
    );
}

#[tokio::test]
async fn s056_acp_without_injected_runtime_fails_before_native_environment_is_built() {
    let native_cell_built = Arc::new(AtomicBool::new(false));
    let native_tools_registered = Arc::new(AtomicBool::new(false));
    let factory = s056_factory(
        None,
        Arc::new(NoopActivitySink),
        Arc::clone(&native_cell_built),
        Arc::clone(&native_tools_registered),
    );

    let err = factory
        .create_task(
            s056_spawn_config(),
            CancellationToken::new(Duration::from_millis(50)),
        )
        .await
        .expect_err("ACP child without runtime must fail before native execution");

    assert!(matches!(
        err,
        RuntimeError::AcpChildRuntimeMissing {
            placement,
            acp_profile,
        } if placement == "reviewer" && acp_profile == "codex-local"
    ));
    assert!(!native_cell_built.load(Ordering::SeqCst));
    assert!(!native_tools_registered.load(Ordering::SeqCst));
}

#[tokio::test]
async fn s056_acp_runtime_receives_cancellation_token() {
    let observed_cancellation = Arc::new(AtomicBool::new(false));
    let observed_for_runtime = Arc::clone(&observed_cancellation);
    let runtime = ScriptedAcpRuntime::new(
        move |_request, cancellation, _activity_sink, _input_queue| {
            let observed = Arc::clone(&observed_for_runtime);
            Box::pin(async move {
                while !cancellation.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                observed.store(true, Ordering::SeqCst);
                Ok(s056_acp_output(
                    ExitReason::Cancelled,
                    "cancelled by parent",
                    TokenUsage::default(),
                    0,
                ))
            })
        },
    );

    let factory = s056_factory(
        Some(runtime),
        Arc::new(NoopActivitySink),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let token = CancellationToken::new(Duration::from_millis(50));
    let run = tokio::spawn(factory.create_task(s056_spawn_config(), token.clone()));

    tokio::time::sleep(Duration::from_millis(20)).await;
    token.signal();

    let output = tokio::time::timeout(Duration::from_secs(1), run)
        .await
        .expect("ACP runtime should observe cancellation promptly")
        .expect("task join should succeed")
        .expect("ACP runtime should return terminal output");

    assert_eq!(output.exit_reason, ExitReason::Cancelled);
    assert!(observed_cancellation.load(Ordering::SeqCst));
}

#[tokio::test]
async fn s056_terminal_summary_counts_acp_activity_derived_tool_uses_without_prose_parsing() {
    let runtime = ScriptedAcpRuntime::new(
        move |_request, _cancellation, activity_sink, _input_queue| {
            Box::pin(async move {
                activity_sink.emit(ActivityEvent::ToolStart {
                    tool_call_id: "acp-tool-1".into(),
                    name: "remote_search".into(),
                    arguments: serde_json::json!({"query": "S056"}),
                });
                activity_sink.emit(ActivityEvent::ToolFinish {
                    tool_call_id: "acp-tool-1".into(),
                    name: "remote_search".into(),
                    is_error: false,
                    duration_ms: 3,
                    exit_code: None,
                });
                Ok(s056_acp_output(
                    ExitReason::Complete,
                    "I used remote_search once, but this prose must not be parsed for counts.",
                    TokenUsage {
                        input_tokens: 5,
                        output_tokens: 8,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    1,
                ))
            })
        },
    );

    let (activity_tx, mut activity_rx) = tokio::sync::mpsc::unbounded_channel();
    let activity_sink: Arc<dyn ActivitySink> = Arc::new(ChannelActivitySink::new(activity_tx));
    let factory = Arc::new(s056_factory(
        Some(runtime),
        Arc::clone(&activity_sink),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    ));

    let mut supervisor = AgentSupervisor::with_task_factory(
        s056_parent_capability(),
        ResourceBudget::new(10_000, 20, Decimal::ZERO, 4),
        factory,
    );
    supervisor.set_root_agent_id(AgentId("parent-1".into()));
    supervisor.set_journal_storage(Arc::new(InMemoryJournalStorage::new()));
    supervisor.set_activity_sink(Arc::clone(&activity_sink));

    let (supervisor_tx, supervisor_rx) = tokio::sync::mpsc::channel(8);
    let supervisor_task = tokio::spawn(async move {
        supervisor.run_actor_loop(supervisor_rx).await;
    });

    let spawn_tool = SpawnAgentTool {
        sender: supervisor_tx.clone(),
        allowed_placements: vec!["reviewer".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-1".into()),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(
            10_000,
            20,
            Decimal::ZERO,
            4,
        ))),
        guidance: None,
    };
    let join_tool = JoinChildAgentTool {
        sender: supervisor_tx.clone(),
        caller_id: AgentId("parent-1".into()),
    };

    let spawn = spawn_tool
        .call(
            serde_json::json!({
                "placement": "reviewer",
                "task": "review the patch",
                "budget": {
                    "max_tokens": 321,
                    "max_turns": 7,
                    "max_cost": "0",
                    "max_sub_agents": 1
                }
            }),
            &s056_parent_capability(),
        )
        .await
        .expect("spawn_agent should accept ACP child");
    let child_id = spawn
        .get("child_id")
        .and_then(|value| value.as_str())
        .expect("spawn response should include child_id")
        .to_string();

    let terminal = tokio::time::timeout(
        Duration::from_secs(1),
        join_tool.call(
            serde_json::json!({ "child_id": child_id }),
            &s056_parent_capability(),
        ),
    )
    .await
    .expect("join_child_agent should not hang")
    .expect("join_child_agent should return terminal summary");

    let mut saw_forwarded_tool_start = false;
    while let Ok(event) = activity_rx.try_recv() {
        if let ActivityEvent::ChildActivity { event, .. } = event
            && matches!(*event, ActivityEvent::ToolStart { .. })
        {
            saw_forwarded_tool_start = true;
        }
    }

    assert!(saw_forwarded_tool_start);
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["token_usage"]["input_tokens"], 5);
    assert_eq!(terminal["token_usage"]["output_tokens"], 8);
    assert_eq!(
        terminal["tool_uses"], 1,
        "ACP terminal summary must count protocol-visible tool activity when no Tool-role messages are returned"
    );

    drop(spawn_tool);
    drop(join_tool);
    drop(supervisor_tx);
    supervisor_task
        .await
        .expect("supervisor task should exit cleanly");
}

// S061 — Path-Shaped Child Ids from `task_name`.

fn s061_spawn_tool(
    parent_id: &str,
) -> (
    SpawnAgentTool,
    tokio::sync::mpsc::Receiver<SupervisorMessage>,
) {
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    (
        SpawnAgentTool {
            sender,
            allowed_placements: vec!["reviewer".into()],
            activity_sink: Arc::new(NoopActivitySink),
            parent_id: AgentId(parent_id.into()),
            parent_budget: Arc::new(Mutex::new(ResourceBudget::new(
                10_000,
                100,
                Decimal::ZERO,
                100,
            ))),
            guidance: None,
        },
        receiver,
    )
}

fn s061_capability() -> CapabilityToken {
    CapabilityToken {
        spawn_placements: vec!["reviewer".into()],
        ..Default::default()
    }
}

fn s061_arguments(task: &str, task_name: Option<serde_json::Value>) -> serde_json::Value {
    let mut arguments = serde_json::json!({
        "placement": "reviewer",
        "task": task,
        "budget": {
            "max_tokens": 10,
            "max_turns": 1,
            "max_cost": "0",
            "max_sub_agents": 1
        }
    });
    if let Some(task_name) = task_name {
        arguments
            .as_object_mut()
            .expect("S061 arguments should be an object")
            .insert("task_name".into(), task_name);
    }
    arguments
}

fn s061_arguments_with_task_name(task: &str, task_name: &str) -> serde_json::Value {
    s061_arguments(task, Some(serde_json::Value::String(task_name.into())))
}

async fn s061_call_and_capture_spawn(
    tool: &SpawnAgentTool,
    receiver: &mut tokio::sync::mpsc::Receiver<SupervisorMessage>,
    arguments: serde_json::Value,
) -> (serde_json::Value, SpawnConfig) {
    let capability = s061_capability();
    let call = tool.call(arguments, &capability);
    tokio::pin!(call);

    let message = tokio::select! {
        result = &mut call => {
            panic!("spawn_agent returned before submitting a spawn request: {result:?}");
        }
        message = receiver.recv() => {
            message.expect("spawn_agent should submit a supervisor request")
        }
    };

    let config = match message.payload {
        SupervisorPayload::Spawn(config, result_tx) => {
            let config = *config;
            result_tx
                .send(Ok(crate::supervisor::SpawnAck {
                    child_id: config.agent_id.clone(),
                    placement: config.placement.clone(),
                    backend: AgentBackend::Native,
                }))
                .expect("spawn_agent should await the acknowledgement");
            config
        }
        other => panic!("expected spawn request, got {other:?}"),
    };

    let acknowledgement = call.await.expect("spawn_agent should accept the spawn");
    (acknowledgement, config)
}

async fn s061_call_and_reject_spawn(
    tool: &SpawnAgentTool,
    receiver: &mut tokio::sync::mpsc::Receiver<SupervisorMessage>,
    arguments: serde_json::Value,
    error: RuntimeError,
) -> (simulacra_types::ToolError, SpawnConfig) {
    let capability = s061_capability();
    let call = tool.call(arguments, &capability);
    tokio::pin!(call);

    let message = tokio::select! {
        result = &mut call => {
            panic!("spawn_agent returned before submitting a spawn request: {result:?}");
        }
        message = receiver.recv() => {
            message.expect("spawn_agent should submit a supervisor request")
        }
    };

    let config = match message.payload {
        SupervisorPayload::Spawn(config, result_tx) => {
            let config = *config;
            result_tx
                .send(Err(error))
                .expect("spawn_agent should await the acknowledgement");
            config
        }
        other => panic!("expected spawn request, got {other:?}"),
    };

    let error = call
        .await
        .expect_err("spawn_agent should propagate supervisor rejection");
    (error, config)
}

async fn s061_expect_invalid_arguments(
    parent_id: &str,
    arguments: serde_json::Value,
    expected_fragments: &[&str],
) {
    let (tool, mut receiver) = s061_spawn_tool(parent_id);
    s061_expect_invalid_arguments_with_tool(&tool, &mut receiver, arguments, expected_fragments)
        .await;
}

async fn s061_expect_invalid_arguments_with_tool(
    tool: &SpawnAgentTool,
    receiver: &mut tokio::sync::mpsc::Receiver<SupervisorMessage>,
    arguments: serde_json::Value,
    expected_fragments: &[&str],
) {
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        tool.call(arguments, &s061_capability()),
    )
    .await
    .expect("argument validation should finish without supervisor acknowledgement")
    .expect_err("spawn_agent should reject invalid arguments");

    let message = match error {
        simulacra_types::ToolError::InvalidArguments(message) => message,
        other => panic!("expected InvalidArguments, got {other:?}"),
    };

    for fragment in expected_fragments {
        assert!(
            message.contains(fragment),
            "expected InvalidArguments message {message:?} to contain {fragment:?}"
        );
    }
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

fn s061_is_legacy_child_id(value: &str) -> bool {
    value.strip_prefix("child-").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .chars()
                .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch))
    })
}

#[test]
fn s061_schema_exposes_optional_task_name_and_preserves_required_fields() {
    let (tool, _receiver) = s061_spawn_tool("s061-parent-schema");
    let schema = tool.definition().input_schema;
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .expect("spawn_agent schema should define object properties");
    let task_name = properties
        .get("task_name")
        .expect("spawn_agent schema should expose task_name");

    assert_eq!(
        task_name.get("type").and_then(|value| value.as_str()),
        Some("string")
    );
    let description = task_name
        .get("description")
        .and_then(|value| value.as_str())
        .expect("task_name should describe valid segment syntax");
    for fragment in ["lowercase", "digits", "underscores"] {
        assert!(
            description.contains(fragment),
            "task_name description should mention {fragment:?}: {description:?}"
        );
    }
    assert_eq!(
        schema.get("required"),
        Some(&serde_json::json!(["placement", "task", "budget"]))
    );
}

#[tokio::test]
async fn s061_task_name_passes_shape_validation_while_unknown_keys_still_fail() {
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-shape-task-name");
    let (acknowledgement, config) = s061_call_and_capture_spawn(
        &tool,
        &mut receiver,
        s061_arguments_with_task_name("review the focused change", "shape_validation"),
    )
    .await;

    assert_eq!(config.agent_id, AgentId("/forge/shape_validation".into()));
    assert_eq!(acknowledgement["child_id"], "/forge/shape_validation");

    let mut unknown = s061_arguments("review the focused change", None);
    unknown
        .as_object_mut()
        .expect("S061 arguments should be an object")
        .insert("unexpected".into(), serde_json::json!(true));
    s061_expect_invalid_arguments(
        "s061-parent-shape-unknown",
        unknown,
        &["unexpected", "unknown"],
    )
    .await;
}

#[tokio::test]
async fn s061_non_string_task_name_fails_as_invalid_arguments() {
    s061_expect_invalid_arguments(
        "s061-parent-non-string-task-name",
        s061_arguments("review the focused change", Some(serde_json::json!(7))),
        &["task_name", "string"],
    )
    .await;
}

#[tokio::test]
async fn s061_blank_task_names_fail_as_invalid_arguments() {
    for (index, task_name) in ["", "   ", "\t\n"].into_iter().enumerate() {
        s061_expect_invalid_arguments(
            &format!("s061-parent-blank-task-name-{index}"),
            s061_arguments_with_task_name("review the focused change", task_name),
            &["task_name", "non-blank"],
        )
        .await;
    }
}

#[tokio::test]
async fn s061_task_name_rejects_invalid_path_segment_characters() {
    for (index, task_name) in [
        "has/slash",
        "Uppercase",
        "has-hyphen",
        "has space",
        "has.dot",
        "emoji_smile_🙂",
    ]
    .into_iter()
    .enumerate()
    {
        s061_expect_invalid_arguments(
            &format!("s061-parent-invalid-task-name-chars-{index}"),
            s061_arguments_with_task_name("review the focused change", task_name),
            &["task_name"],
        )
        .await;
    }
}

#[tokio::test]
async fn s061_task_name_rejects_reserved_segments() {
    for (index, task_name) in ["forge", ".", ".."].into_iter().enumerate() {
        s061_expect_invalid_arguments(
            &format!("s061-parent-reserved-task-name-{index}"),
            s061_arguments_with_task_name("review the focused change", task_name),
            &["task_name", "reserved"],
        )
        .await;
    }
}

#[tokio::test]
async fn s061_task_name_accepts_sixty_four_chars_and_rejects_sixty_five() {
    let sixty_four = "a".repeat(64);
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-sixty-four-task-name");
    let (acknowledgement, config) = s061_call_and_capture_spawn(
        &tool,
        &mut receiver,
        s061_arguments_with_task_name("review the focused change", &sixty_four),
    )
    .await;
    let expected = format!("/forge/{sixty_four}");

    assert_eq!(config.agent_id.0, expected);
    assert_eq!(acknowledgement["child_id"], expected);

    let sixty_five = "b".repeat(65);
    s061_expect_invalid_arguments(
        "s061-parent-sixty-five-task-name",
        s061_arguments_with_task_name("review the focused change", &sixty_five),
        &["task_name", "64"],
    )
    .await;
}

#[tokio::test]
async fn s061_root_task_name_composes_forge_path_and_ack_echoes_it() {
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-root-task-name");
    let (acknowledgement, config) = s061_call_and_capture_spawn(
        &tool,
        &mut receiver,
        s061_arguments_with_task_name("explore the codebase", "explore_codebase"),
    )
    .await;

    assert_eq!(config.agent_id, AgentId("/forge/explore_codebase".into()));
    assert_eq!(acknowledgement["child_id"], "/forge/explore_codebase");
}

#[tokio::test]
async fn s061_descendant_task_name_appends_segment_to_parent_path() {
    let (tool, mut receiver) = s061_spawn_tool("/forge/first_phase");
    let (acknowledgement, config) = s061_call_and_capture_spawn(
        &tool,
        &mut receiver,
        s061_arguments_with_task_name("review the focused change", "review_patch"),
    )
    .await;

    assert_eq!(
        config.agent_id,
        AgentId("/forge/first_phase/review_patch".into())
    );
    assert_eq!(
        acknowledgement["child_id"],
        "/forge/first_phase/review_patch"
    );
}

#[tokio::test]
async fn s061_valid_task_name_flows_through_spawn_config_and_ack_unchanged() {
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-unchanged-flow");
    let (acknowledgement, config) = s061_call_and_capture_spawn(
        &tool,
        &mut receiver,
        s061_arguments_with_task_name("summarize risky diffs", "summarize_risky_diffs"),
    )
    .await;

    assert_eq!(config.agent_id.0, "/forge/summarize_risky_diffs");
    assert_eq!(acknowledgement["child_id"], config.agent_id.0);
}

#[tokio::test]
async fn s061_omitted_task_name_derives_slug_from_task_for_root() {
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-auto-slug-basic");
    let (acknowledgement, config) = s061_call_and_capture_spawn(
        &tool,
        &mut receiver,
        s061_arguments("Explore the Codebase for DF-123", None),
    )
    .await;

    assert_eq!(
        config.agent_id,
        AgentId("/forge/explore_the_codebase_for_df123".into())
    );
    assert_eq!(
        acknowledgement["child_id"],
        "/forge/explore_the_codebase_for_df123"
    );
}

#[tokio::test]
async fn s061_auto_slug_collapses_drops_trims_and_truncates() {
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-auto-slug-normalization");
    let (acknowledgement, config) = s061_call_and_capture_spawn(
        &tool,
        &mut receiver,
        s061_arguments(
            "  Hello,\tWORLD!!! café Δ keep__underscores and very long suffix  ",
            None,
        ),
    )
    .await;

    assert_eq!(
        config.agent_id,
        AgentId("/forge/hello_world_caf_keep__underscore".into())
    );
    assert_eq!(config.agent_id.0.len(), "/forge/".len() + 32);
    assert_eq!(
        acknowledgement["child_id"],
        "/forge/hello_world_caf_keep__underscore"
    );
}

#[tokio::test]
async fn s061_auto_slug_truncates_before_uniqueness_suffix() {
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-auto-slug-truncate-suffix");
    let long_task = "  Hello,\tWORLD!!! café Δ keep__underscores and very long suffix  ";
    let (_, first) =
        s061_call_and_capture_spawn(&tool, &mut receiver, s061_arguments(long_task, None)).await;
    let (_, second) =
        s061_call_and_capture_spawn(&tool, &mut receiver, s061_arguments(long_task, None)).await;

    assert_eq!(first.agent_id.0, "/forge/hello_world_caf_keep__underscore");
    assert_eq!(
        second.agent_id.0, "/forge/hello_world_caf_keep__underscore_2",
        "the uniqueness suffix must be appended after 32-character truncation"
    );
}

#[tokio::test]
async fn s061_no_slugable_task_uses_legacy_child_id_shape() {
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-no-slug-legacy");
    let (acknowledgement, config) =
        s061_call_and_capture_spawn(&tool, &mut receiver, s061_arguments("???", None)).await;
    let child_id = acknowledgement
        .get("child_id")
        .and_then(|value| value.as_str())
        .expect("spawn acknowledgement should include child_id");

    assert_eq!(config.agent_id.0, child_id);
    assert!(
        s061_is_legacy_child_id(child_id),
        "expected legacy fallback id child-[0-9a-f]{{32}}, got {child_id:?}"
    );
}

#[tokio::test]
async fn s061_two_no_slug_spawns_under_one_parent_get_distinct_legacy_ids() {
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-no-slug-distinct");
    let (_, first) =
        s061_call_and_capture_spawn(&tool, &mut receiver, s061_arguments("???", None)).await;
    let (_, second) =
        s061_call_and_capture_spawn(&tool, &mut receiver, s061_arguments("!!!", None)).await;

    assert!(s061_is_legacy_child_id(&first.agent_id.0));
    assert!(s061_is_legacy_child_id(&second.agent_id.0));
    assert_ne!(first.agent_id, second.agent_id);
}

#[tokio::test]
async fn s061_omitted_task_name_dedupes_locally_under_one_parent() {
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-auto-slug-dedupe");
    let (_, first) =
        s061_call_and_capture_spawn(&tool, &mut receiver, s061_arguments("Review patch", None))
            .await;
    let (_, second) =
        s061_call_and_capture_spawn(&tool, &mut receiver, s061_arguments("Review patch", None))
            .await;
    let (_, third) =
        s061_call_and_capture_spawn(&tool, &mut receiver, s061_arguments("Review patch", None))
            .await;

    assert_eq!(first.agent_id.0, "/forge/review_patch");
    assert_eq!(second.agent_id.0, "/forge/review_patch_2");
    assert_eq!(third.agent_id.0, "/forge/review_patch_3");
}

#[tokio::test]
async fn s061_same_auto_slug_under_different_parents_is_unsuffixed_for_both() {
    let (first_tool, mut first_receiver) = s061_spawn_tool("s061-parent-cross-parent-a");
    let (second_tool, mut second_receiver) = s061_spawn_tool("s061-parent-cross-parent-b");

    let (_, first) = s061_call_and_capture_spawn(
        &first_tool,
        &mut first_receiver,
        s061_arguments("Shared slug", None),
    )
    .await;
    let (_, second) = s061_call_and_capture_spawn(
        &second_tool,
        &mut second_receiver,
        s061_arguments("Shared slug", None),
    )
    .await;

    assert_eq!(first.agent_id.0, "/forge/shared_slug");
    assert_eq!(second.agent_id.0, "/forge/shared_slug");
}

#[tokio::test]
async fn s061_model_supplied_duplicate_path_propagates_supervisor_rejection() {
    let duplicate_path = "/forge/duplicate_path";
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-duplicate-model-name");
    let arguments = s061_arguments_with_task_name("review duplicate handling", "duplicate_path");
    let (_, first) = s061_call_and_capture_spawn(&tool, &mut receiver, arguments.clone()).await;
    assert_eq!(first.agent_id.0, duplicate_path);

    let (error, second) = s061_call_and_reject_spawn(
        &tool,
        &mut receiver,
        arguments,
        RuntimeError::CapabilityViolation(format!("already accepted child id {duplicate_path}")),
    )
    .await;
    assert_eq!(second.agent_id.0, duplicate_path);

    let message = match error {
        simulacra_types::ToolError::ExecutionFailed(message) => message,
        other => panic!("expected ExecutionFailed, got {other:?}"),
    };
    assert!(
        message.contains(duplicate_path),
        "ExecutionFailed should contain duplicate path {duplicate_path:?}: {message:?}"
    );
}

#[tokio::test]
async fn s061_auto_slug_drops_non_ascii_whitespace() {
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-auto-slug-non-ascii");
    let (_, config) = s061_call_and_capture_spawn(
        &tool,
        &mut receiver,
        s061_arguments("a\u{00a0}b c\u{2003}d", None),
    )
    .await;

    assert_eq!(config.agent_id.0, "/forge/ab_cd");
}

#[tokio::test]
async fn s061_concurrent_identical_auto_slugs_receive_unique_suffixes() {
    let (tool, mut receiver) = s061_spawn_tool("s061-parent-concurrent-same-slug");
    let tool = Arc::new(tool);
    let capability = s061_capability();
    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let tool = Arc::clone(&tool);
        let capability = capability.clone();
        calls.spawn(async move {
            tool.call(s061_arguments("Same concurrent slug", None), &capability)
                .await
        });
    }

    let mut ids = std::collections::BTreeSet::new();
    for _ in 0..8 {
        let message = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("spawn request should arrive")
            .expect("supervisor channel should stay open");
        match message.payload {
            SupervisorPayload::Spawn(config, result_tx) => {
                let child_id = config.agent_id.0.clone();
                result_tx
                    .send(Ok(crate::supervisor::SpawnAck {
                        child_id: config.agent_id.clone(),
                        placement: config.placement.clone(),
                        backend: AgentBackend::Native,
                    }))
                    .expect("spawn_agent should await the acknowledgement");
                ids.insert(child_id);
            }
            other => panic!("expected spawn request, got {other:?}"),
        }
    }
    while let Some(joined) = calls.join_next().await {
        joined
            .expect("spawn call should not panic")
            .expect("concurrent identical-slug spawn should be accepted");
    }

    let expected: std::collections::BTreeSet<String> = (1..=8)
        .map(|suffix| match suffix {
            1 => "/forge/same_concurrent_slug".to_string(),
            n => format!("/forge/same_concurrent_slug_{n}"),
        })
        .collect();
    assert_eq!(ids, expected);
}

struct S061CompletingFactory;

impl TaskFactory for S061CompletingFactory {
    fn validate_spawn_config(&self, _config: &SpawnConfig) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn create_task(&self, _config: SpawnConfig, _cancellation: CancellationToken) -> BoxTaskFuture {
        Box::pin(async move {
            Ok(AgentLoopOutput {
                exit_reason: ExitReason::Complete,
                messages: Vec::new(),
                token_usage: TokenUsage::default(),
                reported_tool_uses: None,
                used_turns: 0,
                used_cost: Decimal::ZERO,
            })
        })
    }
}

#[tokio::test]
async fn s061_real_supervisor_rejects_duplicate_model_name_and_journals_the_path() {
    let parent_id = AgentId("s061-real-supervisor-root".into());
    let journal = Arc::new(InMemoryJournalStorage::new());
    let mut supervisor = AgentSupervisor::with_task_factory(
        s061_capability(),
        ResourceBudget::new(10_000, 100, Decimal::ZERO, 100),
        Arc::new(S061CompletingFactory),
    );
    supervisor.set_root_agent_id(parent_id.clone());
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    let (supervisor_tx, supervisor_rx) = tokio::sync::mpsc::channel(8);
    let supervisor_task = tokio::spawn(async move {
        supervisor.run_actor_loop(supervisor_rx).await;
    });
    let tool = SpawnAgentTool {
        sender: supervisor_tx.clone(),
        allowed_placements: vec!["reviewer".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: parent_id.clone(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(
            10_000,
            100,
            Decimal::ZERO,
            100,
        ))),
        guidance: None,
    };
    let capability = s061_capability();
    let arguments = s061_arguments_with_task_name("review duplicate handling", "duplicate_real");

    let first = tool
        .call(arguments.clone(), &capability)
        .await
        .expect("first spawn should be accepted");
    assert_eq!(first["child_id"], "/forge/duplicate_real");

    let error = tool
        .call(arguments, &capability)
        .await
        .expect_err("duplicate model-supplied name must be rejected");
    let message = match error {
        simulacra_types::ToolError::ExecutionFailed(message) => message,
        other => panic!("expected ExecutionFailed, got {other:?}"),
    };
    assert!(
        message.contains("/forge/duplicate_real"),
        "rejection should name the duplicate path: {message:?}"
    );
    assert!(
        message.contains("already accepted"),
        "rejection should carry the supervisor's already-accepted vocabulary: {message:?}"
    );

    drop(supervisor_tx);
    drop(tool);
    supervisor_task
        .await
        .expect("supervisor should exit after channel close");

    let entries = journal
        .read_all(&parent_id)
        .expect("parent journal should remain readable");
    let spawned: Vec<String> = entries
        .iter()
        .filter_map(|entry| match &entry.entry {
            JournalEntryKind::SubAgentSpawned { child_id, .. } => Some(child_id.0.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        spawned,
        vec!["/forge/duplicate_real".to_string()],
        "the journal must carry the exact path exactly once; the duplicate must not journal"
    );
}

#[tokio::test]
async fn s061_regression_spawn_without_task_name_still_preserves_s060_validation_order() {
    // One parent for the whole test: a rejected call must not consume the
    // auto-slug segment, so the subsequent valid spawn still receives the
    // unsuffixed path.
    let parent = "s061-parent-regression-order";
    let (tool, mut receiver) = s061_spawn_tool(parent);

    let mut unauthorized_placement = s061_arguments("review the focused change", None);
    unauthorized_placement
        .as_object_mut()
        .expect("S061 arguments should be an object")
        .insert("placement".into(), serde_json::json!("writer"));
    s061_expect_invalid_arguments_with_tool(
        &tool,
        &mut receiver,
        unauthorized_placement,
        &["placement", "unauthorized"],
    )
    .await;

    let mut invalid_budget = s061_arguments("review the focused change", None);
    invalid_budget
        .get_mut("budget")
        .and_then(serde_json::Value::as_object_mut)
        .expect("S061 budget should be an object")
        .insert("max_cost".into(), serde_json::json!("-1"));
    s061_expect_invalid_arguments_with_tool(
        &tool,
        &mut receiver,
        invalid_budget,
        &["max_cost", "nonnegative"],
    )
    .await;

    let (acknowledgement, config) = s061_call_and_capture_spawn(
        &tool,
        &mut receiver,
        s061_arguments("review the focused change", None),
    )
    .await;
    assert_eq!(config.agent_id.0, "/forge/review_the_focused_change");
    assert_eq!(
        acknowledgement["child_id"],
        "/forge/review_the_focused_change"
    );
}
