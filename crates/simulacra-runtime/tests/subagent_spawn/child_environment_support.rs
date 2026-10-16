// Scripted-provider harness for the child environment hook tests: a provider
// that records what it is asked, a hook builder, and a supervisor harness.

type ChildEnvRequests = Arc<Mutex<Vec<Vec<Message>>>>;

type ChildEnvFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<simulacra_runtime::ChildFsEnvironment, RuntimeError>,
            > + Send,
    >,
>;

const CHILD_ENV_MARKER: &str = "/workspace/marker.txt";

/// Pops scripted responses and keeps every message list it was asked with.
struct ChildEnvScriptedProvider {
    responses: Mutex<Vec<ProviderResponse>>,
    seen: ChildEnvRequests,
}

impl Provider for ChildEnvScriptedProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.seen.lock().unwrap().push(messages.to_vec());
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop()
                .expect("scripted child provider should have a response left"))
        })
    }
}

fn child_env_assistant(
    content: &str,
    tool_calls: Vec<ToolCallMessage>,
    finish_reason: FinishReason,
) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            provider_content: vec![],
        },
        token_usage: TokenUsage::default(),
        finish_reason,
        provider_response_id: Some("child-environment".into()),
        model: "child-model".into(),
    }
}

/// One child's script: a single tool call, then finish. Responses are popped
/// from the end, so they are listed last-first.
fn child_env_call_then_finish(name: &str, arguments: serde_json::Value) -> Vec<ProviderResponse> {
    vec![
        child_env_assistant("done", vec![], FinishReason::EndTurn),
        child_env_assistant(
            "",
            vec![ToolCallMessage {
                id: format!("call-{name}"),
                name: name.into(),
                arguments,
            }],
            FinishReason::ToolUse,
        ),
    ]
}

fn child_env_read_marker(_child_index: usize) -> Vec<ProviderResponse> {
    child_env_call_then_finish("file_read", serde_json::json!({ "path": CHILD_ENV_MARKER }))
}

/// Wraps the host's decision in the async hook shape. The environment is
/// ready only after a yield, and after `gate_for`'s receiver (if it hands one
/// out for this spawn) fires, so a factory that polls the hook without
/// awaiting it would get no answer.
fn child_env_gated_provider(
    gate_for: impl Fn(&SpawnConfig) -> Option<tokio::sync::oneshot::Receiver<()>>
    + Send
    + Sync
    + 'static,
    decide: impl Fn(&SpawnConfig) -> Result<simulacra_runtime::ChildFsEnvironment, RuntimeError>
    + Send
    + Sync
    + 'static,
) -> simulacra_runtime::ChildEnvironmentProvider {
    Arc::new(move |spawn: &SpawnConfig| {
        let gate = gate_for(spawn);
        let decided = decide(spawn);
        let future: ChildEnvFuture = Box::pin(async move {
            tokio::task::yield_now().await;
            if let Some(gate) = gate {
                gate.await.expect("the test should release the gate");
            }
            decided
        });
        future
    })
}

fn child_env_provider(
    decide: impl Fn(&SpawnConfig) -> Result<simulacra_runtime::ChildFsEnvironment, RuntimeError>
    + Send
    + Sync
    + 'static,
) -> simulacra_runtime::ChildEnvironmentProvider {
    child_env_gated_provider(|_| None, decide)
}

/// A filesystem holding only the marker, whose content is `text`.
fn child_env_vfs_with_marker(text: &str) -> Arc<dyn VirtualFs> {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.write(CHILD_ENV_MARKER, text.as_bytes())
        .expect("marker should be written");
    vfs
}

fn child_env_marker_for(spawn: &SpawnConfig) -> String {
    format!("for {}", spawn.agent_id.0)
}

fn child_env_harness_with_script(
    provider: Option<simulacra_runtime::ChildEnvironmentProvider>,
    factory_vfs: Arc<dyn VirtualFs>,
    script: impl Fn(usize) -> Vec<ProviderResponse> + Send + Sync + 'static,
    seen: ChildEnvRequests,
) -> ChildCellIdentityHarness {
    let capability = child_cell_identity_capability();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let parent_id = AgentId("root-child-environment".into());
    let constructed = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(AgentTaskFactory {
        config: child_cell_identity_config(),
        provider_kind: ProviderKind::OpenAI,
        vfs: factory_vfs,
        journal: Arc::clone(&journal) as Arc<dyn JournalStorage>,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: capability.clone(),
        allowed_mcp_servers: None,
        supervisor_sender: None,
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_environment_provider: provider,
        child_provider_factory: Some(Arc::new(move |_kind, _model| {
            let index = constructed.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(ChildEnvScriptedProvider {
                responses: Mutex::new(script(index)),
                seen: Arc::clone(&seen),
            }))
        })),
        acp_child_runtime: None,
    });
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        4_096,
        128,
        Decimal::ZERO,
        4,
    )));
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&budget),
        factory,
    );
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    supervisor.set_root_agent_id(parent_id.clone());
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    ChildCellIdentityHarness {
        spawn: SpawnAgentTool {
            sender: sender.clone(),
            allowed_placements: vec!["workspace".into()],
            activity_sink: Arc::new(NoopActivitySink),
            parent_id: parent_id.clone(),
            parent_budget: budget,
            guidance: None,
        },
        join: JoinChildAgentTool {
            sender: sender.clone(),
            caller_id: parent_id,
        },
        capability,
        journal,
        actor,
        sender,
    }
}

/// A harness whose children read the marker. The factory filesystem holds
/// "factory-shared" there, so a child that ignores the hook reads something
/// no hook ever supplies.
fn child_env_harness(
    provider: Option<simulacra_runtime::ChildEnvironmentProvider>,
    seen: ChildEnvRequests,
) -> ChildCellIdentityHarness {
    child_env_harness_with_script(
        provider,
        child_env_vfs_with_marker("factory-shared"),
        child_env_read_marker,
        seen,
    )
}

/// Spawns one child and returns its minted id, without waiting for it.
async fn child_env_spawn(
    harness: &ChildCellIdentityHarness,
    task: &str,
    task_name: &str,
) -> AgentId {
    let acknowledgement = harness
        .spawn
        .call(
            serde_json::json!({
                "placement": "workspace",
                "task": task,
                "task_name": task_name,
                "budget": s060_budget(64, 4, "0", 1)
            }),
            &harness.capability,
        )
        .await
        .expect("native spawn should be acknowledged");
    AgentId(
        acknowledgement["child_id"]
            .as_str()
            .expect("spawn acknowledgement should include child_id")
            .to_string(),
    )
}

async fn child_env_join(
    harness: &ChildCellIdentityHarness,
    child_id: &AgentId,
) -> serde_json::Value {
    harness
        .join
        .call(
            serde_json::json!({ "child_id": child_id.0 }),
            &harness.capability,
        )
        .await
        .expect("join should return the child's terminal result")
}

async fn child_env_spawn_and_join(
    harness: &ChildCellIdentityHarness,
    task_name: &str,
) -> (AgentId, serde_json::Value) {
    let child_id = child_env_spawn(harness, "run the scripted child", task_name).await;
    let joined = child_env_join(harness, &child_id).await;
    (child_id, joined)
}

/// The tool message that ended each request the child's provider saw, in order.
fn child_env_tool_results(seen: &ChildEnvRequests) -> Vec<Message> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|messages| messages.last())
        .filter(|last| last.role == Role::Tool)
        .cloned()
        .collect()
}

fn child_env_tool_messages(seen: &ChildEnvRequests) -> Vec<String> {
    child_env_tool_results(seen)
        .into_iter()
        .map(|message| message.content)
        .collect()
}

/// Records each request and the bytes it was handed, and answers `output`.
struct ChildEnvPresenter {
    output: simulacra_types::ToolOutput,
    seen: Mutex<Vec<(simulacra_tool::FileReadRequest, Vec<u8>)>>,
}

impl ChildEnvPresenter {
    fn answering(output: simulacra_types::ToolOutput) -> Arc<Self> {
        Arc::new(Self {
            output,
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl simulacra_tool::FilePresenter for ChildEnvPresenter {
    fn present<'a>(
        &'a self,
        request: simulacra_tool::FileReadRequest,
        bytes: Vec<u8>,
    ) -> simulacra_tool::PresentFuture<'a> {
        Box::pin(async move {
            self.seen.lock().unwrap().push((request, bytes));
            Ok(self.output.clone())
        })
    }
}

fn child_env_marker_request() -> simulacra_tool::FileReadRequest {
    simulacra_tool::FileReadRequest {
        path: CHILD_ENV_MARKER.into(),
        offset: None,
        limit: None,
    }
}
