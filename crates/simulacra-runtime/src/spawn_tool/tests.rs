use super::*;
use crate::{
    AgentSupervisor, ChannelActivitySink, InMemoryJournalStorage, NoopActivitySink,
    RestartStrategy, TaskFactory,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use simulacra_hooks::{HookError, HookModule, HookPipeline, Operation, Phase, Verdict};
use simulacra_types::{
    ActivityEvent, ExitReason, FsMetadata, MemoryCapability, MemoryPath, PathPattern, Role,
    TokenUsage, Tool, VfsError, VfsSnapshot,
};
use simulacra_vfs::MemoryFs;

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

type ScriptedAcpHandler = dyn Fn(AcpChildRequest, CancellationToken, Arc<dyn ActivitySink>) -> crate::AcpChildFuture
    + Send
    + Sync;

struct ScriptedAcpRuntime {
    handler: Arc<ScriptedAcpHandler>,
}

impl ScriptedAcpRuntime {
    fn new<F>(handler: F) -> Arc<Self>
    where
        F: Fn(AcpChildRequest, CancellationToken, Arc<dyn ActivitySink>) -> crate::AcpChildFuture
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
    ) -> crate::AcpChildFuture {
        (self.handler)(request, cancellation, activity_sink)
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

struct RecordingSpawnHook {
    before_verdict: Verdict,
    before_calls: Arc<AtomicUsize>,
    after_calls: Arc<AtomicUsize>,
}

impl HookModule for RecordingSpawnHook {
    fn name(&self) -> &str {
        "recording-spawn-hook"
    }

    fn invoke(
        &self,
        phase: Phase,
        operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError> {
        assert_eq!(operation, Operation::Spawn);
        match phase {
            Phase::Before => {
                self.before_calls.fetch_add(1, Ordering::SeqCst);
                let context: serde_json::Value =
                    serde_json::from_str(context).expect("spawn hook context should be JSON");
                assert_eq!(context["agent_type"], "reviewer");
                assert_eq!(context["budget"]["max_tokens"], 321);
                Ok(self.before_verdict.clone())
            }
            Phase::After => {
                self.after_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Verdict::continue_unchanged())
            }
        }
    }
}

fn s056_acp_config() -> SimulacraConfig {
    let toml_str = r#"
[project]
name = "s056"

[agent_types.reviewer]
backend = "acp"
acp_profile = "codex-local"

[agent_types.reviewer.capabilities]
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
        spawn_types: vec!["reviewer".into()],
        ..Default::default()
    }
}

fn s056_spawn_config() -> SpawnConfig {
    SpawnConfig {
        agent_id: AgentId("child-acp-1".into()),
        parent_id: AgentId("parent-1".into()),
        capability: None,
        budget: ResourceBudget::new(321, 7, Decimal::ZERO, 0),
        restart_strategy: RestartStrategy::LetCrash,
        agent_type: Some("reviewer".into()),
        task: "review the patch".into(),
        system_prompt: None,
        tier: None,
        resolved_tier: None,
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
        supervisor_sender: None,
        parent_model: "parent-model".into(),
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

fn s056_factory_with_pipeline(
    acp_child_runtime: Option<Arc<dyn AcpChildRuntime>>,
    pipeline: Arc<HookPipeline>,
) -> AgentTaskFactory {
    AgentTaskFactory {
        pipeline: Some(pipeline),
        ..s056_factory(
            acp_child_runtime,
            Arc::new(NoopActivitySink),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
    }
}

#[tokio::test]
async fn s056_acp_factory_delegates_request_without_native_environment() {
    let requests = Arc::new(Mutex::new(Vec::<AcpChildRequest>::new()));
    let requests_for_runtime = Arc::clone(&requests);
    let runtime = ScriptedAcpRuntime::new(move |request, cancellation, activity_sink| {
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
    assert_eq!(request.agent_type, "reviewer");
    assert_eq!(request.acp_profile, "codex-local");
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

    assert!(
        matches!(
            err,
            RuntimeError::AcpChildRuntimeMissing {
                ref agent_type,
                ref acp_profile
            } if agent_type == "reviewer" && acp_profile == "codex-local"
        ),
        "unexpected error: {err}"
    );
    assert!(!native_cell_built.load(Ordering::SeqCst));
    assert!(!native_tools_registered.load(Ordering::SeqCst));
}

#[tokio::test]
async fn s056_acp_spawn_runs_simulacra_spawn_hooks_without_native_environment() {
    let before_calls = Arc::new(AtomicUsize::new(0));
    let after_calls = Arc::new(AtomicUsize::new(0));
    let mut pipeline = HookPipeline::new();
    pipeline.add(
        Operation::Spawn,
        Arc::new(RecordingSpawnHook {
            before_verdict: Verdict::continue_unchanged(),
            before_calls: Arc::clone(&before_calls),
            after_calls: Arc::clone(&after_calls),
        }),
    );
    let runtime_started = Arc::new(AtomicBool::new(false));
    let runtime_started_for_runtime = Arc::clone(&runtime_started);
    let runtime = ScriptedAcpRuntime::new(move |_request, _cancellation, _activity_sink| {
        let runtime_started = Arc::clone(&runtime_started_for_runtime);
        Box::pin(async move {
            runtime_started.store(true, Ordering::SeqCst);
            Ok(s056_acp_output(
                ExitReason::Complete,
                "hooked ACP summary",
                TokenUsage::default(),
                1,
            ))
        })
    });

    let factory = s056_factory_with_pipeline(Some(runtime), Arc::new(pipeline));

    let output = factory
        .create_task(
            s056_spawn_config(),
            CancellationToken::new(Duration::from_millis(50)),
        )
        .await
        .expect("ACP child should run when spawn hooks continue");

    assert_eq!(
        output.messages.last().unwrap().content,
        "hooked ACP summary"
    );
    assert!(runtime_started.load(Ordering::SeqCst));
    assert_eq!(before_calls.load(Ordering::SeqCst), 1);
    assert_eq!(after_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn s056_acp_spawn_before_hook_denial_prevents_runtime_start() {
    let before_calls = Arc::new(AtomicUsize::new(0));
    let after_calls = Arc::new(AtomicUsize::new(0));
    let mut pipeline = HookPipeline::new();
    pipeline.add(
        Operation::Spawn,
        Arc::new(RecordingSpawnHook {
            before_verdict: Verdict::Deny("blocked by spawn policy".into()),
            before_calls: Arc::clone(&before_calls),
            after_calls: Arc::clone(&after_calls),
        }),
    );
    let runtime_started = Arc::new(AtomicBool::new(false));
    let runtime_started_for_runtime = Arc::clone(&runtime_started);
    let runtime = ScriptedAcpRuntime::new(move |_request, _cancellation, _activity_sink| {
        let runtime_started = Arc::clone(&runtime_started_for_runtime);
        Box::pin(async move {
            runtime_started.store(true, Ordering::SeqCst);
            Ok(s056_acp_output(
                ExitReason::Complete,
                "should not run",
                TokenUsage::default(),
                1,
            ))
        })
    });

    let factory = s056_factory_with_pipeline(Some(runtime), Arc::new(pipeline));

    let err = factory
        .create_task(
            s056_spawn_config(),
            CancellationToken::new(Duration::from_millis(50)),
        )
        .await
        .expect_err("spawn hook denial should reject ACP child before runtime start");

    assert!(
        matches!(err, RuntimeError::HookDenial(ref reason) if reason == "blocked by spawn policy"),
        "unexpected error: {err}"
    );
    assert!(!runtime_started.load(Ordering::SeqCst));
    assert_eq!(before_calls.load(Ordering::SeqCst), 1);
    assert_eq!(after_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn s056_acp_runtime_receives_cancellation_token() {
    let observed_cancellation = Arc::new(AtomicBool::new(false));
    let observed_for_runtime = Arc::clone(&observed_cancellation);
    let runtime = ScriptedAcpRuntime::new(move |_request, cancellation, _activity_sink| {
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
    });

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
    let runtime = ScriptedAcpRuntime::new(move |_request, _cancellation, activity_sink| {
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
                },
                1,
            ))
        })
    });

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
    supervisor.set_activity_sink(Arc::clone(&activity_sink));

    let (supervisor_tx, supervisor_rx) = tokio::sync::mpsc::channel(8);
    let supervisor_task = tokio::spawn(async move {
        supervisor.run_actor_loop(supervisor_rx).await;
    });

    let spawn_tool = SpawnAgentTool {
        sender: supervisor_tx.clone(),
        can_spawn: vec!["reviewer".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-1".into()),
        tiers: TierMap::new(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(
            10_000,
            20,
            Decimal::ZERO,
            4,
        ))),
        parent_model: "parent-model".into(),
    };
    let join_tool = JoinChildAgentTool {
        sender: supervisor_tx.clone(),
    };

    let spawn = spawn_tool
        .call(
            serde_json::json!({
                "agent_type": "reviewer",
                "task": "review the patch",
                "budget": {
                    "max_tokens": 321,
                    "max_turns": 7,
                    "max_cost": "0",
                    "max_sub_agents": 0
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
