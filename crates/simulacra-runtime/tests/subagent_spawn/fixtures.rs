fn default_capability() -> CapabilityToken {
    CapabilityToken {
        spawn_types: vec!["researcher".into(), "reviewer".into()],
        ..Default::default()
    }
}

fn default_budget() -> ResourceBudget {
    ResourceBudget::new(100, 10, Decimal::new(100, 0), 2)
}

fn child_budget(max_tokens: u64, max_turns: u32, max_sub_agents: u32) -> ResourceBudget {
    child_budget_with_cost(max_tokens, max_turns, Decimal::new(10, 0), max_sub_agents)
}

fn spawn_config(agent_id: &str, parent_id: &str, budget: ResourceBudget) -> SpawnConfig {
    spawn_config_with_agent_type(agent_id, parent_id, "researcher", budget)
}

fn child_budget_with_cost(
    max_tokens: u64,
    max_turns: u32,
    max_cost: Decimal,
    max_sub_agents: u32,
) -> ResourceBudget {
    ResourceBudget::new(max_tokens, max_turns, max_cost, max_sub_agents)
}

fn spawn_config_with_agent_type(
    agent_id: &str,
    parent_id: &str,
    agent_type: &str,
    budget: ResourceBudget,
) -> SpawnConfig {
    SpawnConfig {
        agent_id: AgentId(agent_id.into()),
        parent_id: AgentId(parent_id.into()),
        capability: None,
        budget,
        restart_strategy: RestartStrategy::LetCrash,
        agent_type: Some(agent_type.into()),
        task: "delegate task".into(),
        system_prompt: None,
        tier: None,
        resolved_tier: None,
    }
}

fn child_success_output() -> AgentLoopOutput {
    AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![Message {
            role: Role::Assistant,
            content: "child summary".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }],
        token_usage: TokenUsage {
            input_tokens: 3,
            output_tokens: 2,
        },
            reported_tool_uses: None,
        used_turns: 1,
        used_cost: Decimal::new(15, 2),
    }
}

/// Minimal task factory that immediately resolves with a completed output.
/// Used by tests that validate supervisor-side invariants (budget checks,
/// used_sub_agents increment, spans) without caring about child behaviour.
///
/// `AgentSupervisor::spawn_agent` now requires a task factory (WARNING 1 fix)
/// so tests that previously used `AgentSupervisor::new` must swap to
/// `with_task_factory(..., NoopFactory)`.
struct NoopFactory;

impl TaskFactory for NoopFactory {
    fn create_task(
        &self,
        _config: SpawnConfig,
        _token: simulacra_runtime::CancellationToken,
    ) -> BoxTaskFuture {
        Box::pin(async {
            Ok(AgentLoopOutput {
                exit_reason: ExitReason::Complete,
                messages: vec![],
                token_usage: TokenUsage::default(),
            reported_tool_uses: None,
                used_turns: 0,
                used_cost: Decimal::ZERO,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SpawnSnapshot {
    agent_id: String,
    parent_id: String,
    agent_type: String,
    task: String,
    max_tokens: u64,
    max_turns: u32,
    max_sub_agents: u32,
    restart_strategy: RestartStrategy,
}

#[derive(Clone)]
struct RecordingTaskFactory {
    inner: Arc<RecordingTaskFactoryInner>,
}

struct RecordingTaskFactoryInner {
    outputs: Mutex<VecDeque<Result<AgentLoopOutput, RuntimeError>>>,
    started: Mutex<Vec<SpawnSnapshot>>,
    completed: AtomicUsize,
    started_notify: Notify,
    completed_notify: Notify,
    /// Journal snapshot captured at the moment create_task is called (child execution begins).
    /// This lets tests verify what journal entries existed *before* the child started.
    journal_at_spawn: Mutex<Option<Vec<JournalEntry>>>,
    /// Optional journal reference for capturing state at spawn time.
    journal_ref: Mutex<Option<(Arc<dyn JournalStorage>, AgentId)>>,
}

struct FailingAppendJournal;

impl JournalStorage for FailingAppendJournal {
    fn append(&self, _entry: JournalEntry) -> Result<(), simulacra_types::JournalError> {
        Err(simulacra_types::JournalError::Storage(
            "injected append failure".into(),
        ))
    }

    fn read_all(
        &self,
        _agent_id: &AgentId,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        Ok(vec![])
    }

    fn query_token_usage(
        &self,
        _agent_id: &AgentId,
    ) -> Result<TokenUsage, simulacra_types::JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        _agent_id: &AgentId,
        _after_entry: usize,
        _data: simulacra_types::CheckpointData,
    ) -> Result<(), simulacra_types::JournalError> {
        Ok(())
    }

    fn fork_from(
        &self,
        _agent_id: &AgentId,
        _checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        Ok(vec![])
    }

    fn read_from(
        &self,
        _agent_id: &AgentId,
        _start_index: usize,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        Ok(vec![])
    }
}

impl RecordingTaskFactory {
    fn new(outputs: Vec<Result<AgentLoopOutput, RuntimeError>>) -> Self {
        Self {
            inner: Arc::new(RecordingTaskFactoryInner {
                outputs: Mutex::new(outputs.into()),
                started: Mutex::new(Vec::new()),
                completed: AtomicUsize::new(0),
                started_notify: Notify::new(),
                completed_notify: Notify::new(),
                journal_at_spawn: Mutex::new(None),
                journal_ref: Mutex::new(None),
            }),
        }
    }

    /// Configure the factory to snapshot the journal at spawn time for ordering assertions.
    fn with_journal_capture(self, journal: Arc<dyn JournalStorage>, parent_id: AgentId) -> Self {
        *self.inner.journal_ref.lock().unwrap() = Some((journal, parent_id));
        self
    }

    /// Return the journal entries that existed when create_task was called.
    fn journal_at_spawn_time(&self) -> Option<Vec<JournalEntry>> {
        self.inner.journal_at_spawn.lock().unwrap().clone()
    }

    fn started_count(&self) -> usize {
        self.inner.started.lock().unwrap().len()
    }
    async fn wait_for_completed(&self, expected: usize) {
        loop {
            if self.inner.completed.load(Ordering::SeqCst) >= expected {
                return;
            }
            self.inner.completed_notify.notified().await;
        }
    }
}

impl TaskFactory for RecordingTaskFactory {
    fn create_task(&self, config: SpawnConfig, _cancellation: CancellationToken) -> BoxTaskFuture {
        // Capture journal state at the moment child execution begins.
        if let Some((ref journal, ref parent_id)) = *self.inner.journal_ref.lock().unwrap() {
            let entries = journal.read_all(parent_id).unwrap_or_default();
            *self.inner.journal_at_spawn.lock().unwrap() = Some(entries);
        }

        self.inner.started.lock().unwrap().push(SpawnSnapshot {
            agent_id: config.agent_id.0.clone(),
            parent_id: config.parent_id.0.clone(),
            agent_type: config
                .agent_type
                .clone()
                .unwrap_or_else(|| "generic".to_string()),
            task: config.task.clone(),
            max_tokens: config.budget.max_tokens,
            max_turns: config.budget.max_turns,
            max_sub_agents: config.budget.max_sub_agents,
            restart_strategy: config.restart_strategy.clone(),
        });
        self.inner.started_notify.notify_waiters();

        let output = self
            .inner
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                Ok(AgentLoopOutput {
                    exit_reason: ExitReason::Complete,
                    messages: vec![],
                    token_usage: TokenUsage::default(),
            reported_tool_uses: None,
                    used_turns: 0,
                    used_cost: Decimal::ZERO,
                })
            });
        let factory = self.clone();

        Box::pin(async move {
            let result = output;
            factory.inner.completed.fetch_add(1, Ordering::SeqCst);
            factory.inner.completed_notify.notify_waiters();
            result
        })
    }
}

struct SummarySpawnTool {
    live_calls: Arc<AtomicUsize>,
}

impl Tool for SummarySpawnTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "spawn_agent".into(),
            description: "Delegate work to a child agent.".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn call(
        &self,
        _arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>,
    > {
        let calls = Arc::clone(&self.live_calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({
                "child_id": "child-1",
                "agent_type": "researcher",
                "exit_reason": "completed",
                "message": "done",
                "token_usage": {"input_tokens": 3, "output_tokens": 2}
            }))
        })
    }
}

fn replay_entry(agent_id: &str, entry: JournalEntryKind) -> JournalEntry {
    JournalEntry {
        schema_version: simulacra_types::JOURNAL_SCHEMA_VERSION,
        agent_id: AgentId(agent_id.into()),
        timestamp_ms: 1,
        entry,
    }
}

fn build_loop(
    provider: FakeProvider,
    tools: ToolRegistry,
    replay_journal: Option<Vec<JournalEntry>>,
) -> AgentLoop {
    AgentLoop::with_clock_and_replay(
        AgentLoopConfig {
            agent_id: AgentId("parent-agent".into()),
            system_prompt: "You are a parent.".into(),
            model: "test-model".into(),
            max_turns: 10,
            capability: CapabilityToken::default(),
        },
        Box::new(provider),
        tools,
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        Box::new(simulacra_types::SystemClock),
        replay_journal,
    )
}

fn task_factory_config(child_capabilities: CapabilitiesConfig) -> SimulacraConfig {
    let mut agent_types = HashMap::new();
    agent_types.insert(
        "researcher".into(),
        AgentTypeConfig {
            backend: Default::default(),
            model: "child-model".into(),
            acp_profile: None,
            system_prompt: Some("You are the child researcher.".into()),
            skills: vec![],
            max_turns: Some(3),
            max_tokens: Some(64),
            max_sub_agents: Some(1),
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(child_capabilities),
        },
    );

    SimulacraConfig {
        project: ProjectConfig {
            name: "simulacra-s018-runtime".into(),
            description: None,
        },
        agent_types,
        integrations: HashMap::new(),
        tenants: HashMap::new(),
        mcp: None,
        task: None,
        vfs: VfsConfig::default(),
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: CatalogConfig::default(),
    }
}

async fn run_spawn_tool_call(
    arguments: serde_json::Value,
    can_spawn: &[&str],
    supervisor_reply: Result<AgentLoopOutput, RuntimeError>,
) -> (Result<serde_json::Value, ToolError>, SpawnConfig) {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let tool = SpawnAgentTool {
        sender,
        can_spawn: can_spawn.iter().map(|value| (*value).to_string()).collect(),
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
        guidance: None,
    };
    let call_future = tool.call(arguments, &CapabilityToken::default());
    let receive_future = async move {
        let message = receiver
            .recv()
            .await
            .expect("spawn tool should send one supervisor message");
        assert_eq!(
            message.priority,
            MessagePriority::Command,
            "spawn_agent requests should be sent as command-priority supervisor messages"
        );
        match message.payload {
            SupervisorPayload::Spawn(config, result_tx) => {
                let captured = (*config).clone();
                let reply = supervisor_reply.map(|_| SpawnAck {
                    child_id: captured.agent_id.clone(),
                    agent_type: captured
                        .agent_type
                        .clone()
                        .unwrap_or_else(|| "generic".to_string()),
                });
                result_tx
                    .send(reply)
                    .expect("spawn tool should still be awaiting the spawn acknowledgement");
                captured
            }
            other => panic!("expected SupervisorPayload::Spawn, got {other:?}"),
        }
    };

    let (result, captured) = tokio::join!(call_future, receive_future);
    (result, captured)
}

async fn run_join_tool_call(
    terminal_result: Result<AgentLoopOutput, String>,
) -> Result<serde_json::Value, ToolError> {
    run_join_tool_call_with_metadata(terminal_result, 42, 0).await
}

async fn run_join_tool_call_with_metadata(
    terminal_result: Result<AgentLoopOutput, String>,
    elapsed_ms: u64,
    tool_uses: u64,
) -> Result<serde_json::Value, ToolError> {
    let status = status_from_join_terminal_result(&terminal_result);
    run_join_tool_call_with_status_metadata(terminal_result, status, elapsed_ms, tool_uses).await
}

async fn run_join_tool_call_with_status_metadata(
    terminal_result: Result<AgentLoopOutput, String>,
    status: &str,
    elapsed_ms: u64,
    tool_uses: u64,
) -> Result<serde_json::Value, ToolError> {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let tool = JoinChildAgentTool { sender };
    let call_future = tool.call(
        serde_json::json!({ "child_id": "child-1" }),
        &CapabilityToken::default(),
    );
    let receive_future = async move {
        let message = receiver
            .recv()
            .await
            .expect("join tool should send one supervisor message");
        match message.payload {
            SupervisorPayload::JoinChild(child_id, result_tx) => {
                assert_eq!(child_id.0, "child-1");
                result_tx
                    .send(Ok(ChildTerminalResult {
                        child_id,
                        agent_type: "researcher".into(),
                        status: status.to_string(),
                        elapsed_ms,
                        tool_uses,
                        result: terminal_result,
                    }))
                    .expect("join tool should still be awaiting the terminal result");
            }
            other => panic!("expected SupervisorPayload::JoinChild, got {other:?}"),
        }
    };
    let (result, _) = tokio::join!(call_future, receive_future);
    result
}

fn status_from_join_terminal_result(result: &Result<AgentLoopOutput, String>) -> &'static str {
    match result {
        Ok(output) if output.exit_reason == ExitReason::Cancelled => "cancelled",
        Ok(output) if matches!(output.exit_reason, ExitReason::Error(_)) => "failed",
        Ok(_) => "completed",
        Err(_) => "failed",
    }
}
