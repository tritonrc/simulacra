use simulacra_runtime::SpawnAgentGuidance;

const S060_DEFAULT_SPAWN_DESCRIPTION: &str = "I can start a supervised child for one concrete, bounded, independent task. Choose where I run it with placement and shape how it works with instructions; placement supplies an environment and capabilities, not a role. I return a live handle, not the child's final answer.";
const S060_PLACEMENT_DESCRIPTION_PREFIX: &str = "Where I should run this child and which host-supplied capability envelope it receives. This selects placement, not a role.";
const S060_INSTRUCTIONS_DESCRIPTION: &str = "How I should shape this child for the delegated task, including any relevant available skills and evidence requirements. This does not grant capabilities.";
const S060_TASK_DESCRIPTION: &str = "The concrete, bounded work I should hand to the child.";
const S060_BUDGET_DESCRIPTION: &str = "The maximum resources I should reserve for this child; each nonzero value must fit within my remaining budget and the placement limits, while zero requests unlimited capacity under the rules below.";
const S060_CAPABILITIES_DESCRIPTION: &str = "Capabilities I should remove from this child's placement envelope; these values can only attenuate access.";

fn s060_budget(
    max_tokens: u64,
    max_turns: u32,
    max_cost: &str,
    max_sub_agents: u32,
) -> serde_json::Value {
    serde_json::json!({
        "max_tokens": max_tokens,
        "max_turns": max_turns,
        "max_cost": max_cost,
        "max_sub_agents": max_sub_agents
    })
}

fn s060_capability(placements: &[&str]) -> CapabilityToken {
    CapabilityToken {
        spawn_placements: placements
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        ..CapabilityToken::default()
    }
}

fn s060_spawn_tool(
    allowed_placements: &[&str],
    parent_budget: ResourceBudget,
    guidance: Option<SpawnAgentGuidance>,
) -> (
    SpawnAgentTool,
    tokio::sync::mpsc::Receiver<SupervisorMessage>,
) {
    let (tool, receiver, _budget) =
        s060_spawn_tool_with_budget_handle(allowed_placements, parent_budget, guidance);
    (tool, receiver)
}

fn s060_spawn_tool_with_budget_handle(
    allowed_placements: &[&str],
    parent_budget: ResourceBudget,
    guidance: Option<SpawnAgentGuidance>,
) -> (
    SpawnAgentTool,
    tokio::sync::mpsc::Receiver<SupervisorMessage>,
    Arc<Mutex<ResourceBudget>>,
) {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let parent_budget = Arc::new(Mutex::new(parent_budget));
    let budget_handle = Arc::clone(&parent_budget);
    (
        SpawnAgentTool {
            sender,
            allowed_placements: allowed_placements
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            activity_sink: Arc::new(NoopActivitySink),
            parent_id: AgentId("parent-agent".into()),
            parent_budget,
            guidance,
        },
        receiver,
        budget_handle,
    )
}

async fn s060_call_and_capture(
    tool: &SpawnAgentTool,
    mut receiver: tokio::sync::mpsc::Receiver<SupervisorMessage>,
    arguments: serde_json::Value,
    capability: &CapabilityToken,
) -> (Result<serde_json::Value, ToolError>, Option<SpawnConfig>) {
    let call = tool.call(arguments, capability);
    tokio::pin!(call);

    tokio::select! {
        result = &mut call => (result, None),
        message = receiver.recv() => {
            let message = message.expect("open supervisor channel should receive a dispatched spawn");
            let (config, reply) = match message.payload {
                SupervisorPayload::Spawn(config, reply) => (config, reply),
                other => panic!("expected SupervisorPayload::Spawn, got {other:?}"),
            };
            let captured = (*config).clone();
            reply.send(Ok(SpawnAck {
                child_id: captured.agent_id.clone(),
                placement: captured.placement.clone(),
                backend: AgentBackend::Native,
            }))
            .expect("spawn call should await its acknowledgement");
            (call.await, Some(captured))
        }
    }
}

async fn s060_expect_rejected_before_dispatch(arguments: serde_json::Value, expected_field: &str) {
    let original_budget = ResourceBudget::new(99, 9, Decimal::new(900, 2), 3);
    let (tool, receiver, budget_handle) =
        s060_spawn_tool_with_budget_handle(&["workspace"], original_budget.clone(), None);
    let (result, dispatched) =
        s060_call_and_capture(&tool, receiver, arguments, &s060_capability(&["workspace"])).await;
    let error = result
        .expect_err("malformed spawn must be rejected")
        .to_string();
    assert!(
        error.contains(expected_field),
        "error should name {expected_field:?}; got {error:?}"
    );
    assert!(
        dispatched.is_none(),
        "malformed call reached the supervisor"
    );
    let after = budget_handle.lock().expect("budget lock").clone();
    assert_eq!(
        serde_json::to_value(after).expect("budget serializes"),
        serde_json::to_value(original_budget).expect("budget serializes"),
        "rejection must not reserve or mutate the parent budget"
    );
}

fn s060_definition(allowed_placements: &[&str]) -> ToolDefinition {
    let (tool, _receiver) = s060_spawn_tool(
        allowed_placements,
        ResourceBudget::new(0, 0, Decimal::ZERO, 0),
        None,
    );
    tool.definition()
}

fn s060_parse_runtime_config(source: &str) -> SimulacraConfig {
    let config: SimulacraConfig = toml::from_str(source).expect("S060 fixture TOML should parse");
    config.validate().expect("S060 fixture should validate");
    config
}

fn s060_real_task_factory(config: SimulacraConfig) -> AgentTaskFactory {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("S060 fixture workspace should be created");
    AgentTaskFactory {
        config,
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal: Arc::new(InMemoryJournalStorage::new()),
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: s060_capability(&["workspace", "in_process"]),
        allowed_mcp_servers: None,
        supervisor_sender: None,
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: None,
        acp_child_runtime: None,
    }
}

/// Keeps the production placement resolver in front of the recording fake so
/// this integration test observes both the live supervisor validation seam and
/// the exact request that reaches child construction.
struct S060ValidatingRecordingFactory {
    validator: AgentTaskFactory,
    recorder: RecordingTaskFactory,
}

impl TaskFactory for S060ValidatingRecordingFactory {
    fn validate_spawn_config(&self, config: &SpawnConfig) -> Result<(), RuntimeError> {
        self.validator.validate_spawn_config(config)
    }

    fn create_task(&self, config: SpawnConfig, cancellation: CancellationToken) -> BoxTaskFuture {
        self.recorder.create_task(config, cancellation)
    }
}

struct S060CatalogCapturingProvider {
    captured: Arc<Mutex<Vec<ToolDefinition>>>,
}

impl Provider for S060CatalogCapturingProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        *self.captured.lock().expect("catalog capture lock") = tools.to_vec();
        Box::pin(async {
            Ok(ProviderResponse {
                message: Message {
                    role: Role::Assistant,
                    content: "done".into(),
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_content: vec![],
                },
                token_usage: TokenUsage::default(),
                finish_reason: FinishReason::EndTurn,
                provider_response_id: Some("s060-catalog".into()),
                model: "child-model".into(),
            })
        })
    }
}

fn s060_supervisor_request(placement: &str, budget: ResourceBudget) -> SpawnConfig {
    spawn_config_with_placement(
        "child-0123456789abcdef0123456789abcdef",
        "parent-agent",
        placement,
        budget,
    )
}

// This is the sole assertion seam for caller shaping in the captured
// cross-boundary request. The GREEN migration updates this helper when
// SpawnConfig adopts S060 vocabulary; individual behavioral tests stay
// independent of the Rust field rename.
fn s060_captured_instructions(config: &SpawnConfig) -> Option<&str> {
    config.instructions.as_deref()
}

#[test]
fn s060_a08_spawn_schema_is_flat_and_matches_the_complete_contract() {
    let schema = s060_definition(&["workspace"]).input_schema;
    assert_eq!(schema["type"], "object");
    assert_eq!(
        schema["required"],
        serde_json::json!(["placement", "task", "budget"])
    );
    assert_eq!(schema["additionalProperties"], false);

    let properties = schema["properties"]
        .as_object()
        .expect("top-level properties should be an object");
    let mut property_names = properties.keys().map(String::as_str).collect::<Vec<_>>();
    property_names.sort_unstable();
    assert_eq!(
        property_names,
        vec![
            "budget",
            "capabilities",
            "instructions",
            "placement",
            "task"
        ]
    );

    assert_eq!(
        properties["placement"],
        serde_json::json!({
            "type": "string",
            "description": format!(
                "{S060_PLACEMENT_DESCRIPTION_PREFIX} Available placements: \"workspace\"."
            )
        })
    );
    assert_eq!(
        properties["instructions"],
        serde_json::json!({
            "type": "string",
            "description": S060_INSTRUCTIONS_DESCRIPTION
        })
    );
    assert_eq!(
        properties["task"],
        serde_json::json!({
            "type": "string",
            "description": S060_TASK_DESCRIPTION
        })
    );
    assert_eq!(
        properties["budget"],
        serde_json::json!({
            "type": "object",
            "description": S060_BUDGET_DESCRIPTION,
            "properties": {
                "max_tokens": { "type": "integer", "minimum": 0 },
                "max_turns": { "type": "integer", "minimum": 0 },
                "max_cost": { "type": "string", "description": "The decimal cost limit I should reserve, represented as a string." },
                "max_sub_agents": { "type": "integer", "minimum": 0 }
            },
            "required": ["max_tokens", "max_turns", "max_cost", "max_sub_agents"],
            "additionalProperties": false
        })
    );
    assert_eq!(
        properties["capabilities"],
        serde_json::json!({
            "type": "object",
            "description": S060_CAPABILITIES_DESCRIPTION,
            "properties": {
                "network": { "type": "array", "items": { "type": "string" } },
                "mcp_tools": { "type": "array", "items": { "type": "string" } },
                "shell": { "type": "boolean" },
                "javascript": { "type": "boolean" },
                "python": { "type": "boolean" },
                "paths_write": { "type": "array", "items": { "type": "string" } },
                "paths_read": { "type": "array", "items": { "type": "string" } },
                "spawn_placements": { "type": "array", "items": { "type": "string" } }
            },
            "additionalProperties": false
        })
    );
}

#[test]
fn s060_a09_spawn_schema_omits_role_vocabulary_and_combinator_footguns() {
    let schema = s060_definition(&["workspace"]).input_schema;
    let properties = schema["properties"]
        .as_object()
        .expect("top-level properties should be an object");
    for forbidden in [
        "child_id",
        "agent_type",
        "system_prompt",
        "tier",
        "skills",
        "skill_patterns",
        "memory",
    ] {
        assert!(
            !properties.contains_key(forbidden),
            "schema exposed {forbidden}"
        );
    }
    assert!(schema.pointer("/properties/placement/enum").is_none());
    for combinator in ["oneOf", "anyOf", "allOf"] {
        assert!(
            schema.get(combinator).is_none(),
            "top-level schema emitted {combinator}"
        );
    }
}

#[test]
fn s060_a10_placement_description_is_sorted_deduplicated_and_not_authorization() {
    let populated = s060_definition(&["workspace", "in_process", "workspace"]);
    assert_eq!(
        populated.input_schema["properties"]["placement"]["description"],
        format!(
            "{S060_PLACEMENT_DESCRIPTION_PREFIX} Available placements: \"in_process\", \"workspace\"."
        )
    );
    assert!(
        populated.input_schema["properties"]["placement"]
            .get("enum")
            .is_none()
    );

    let empty = s060_definition(&[]);
    assert_eq!(
        empty.input_schema["properties"]["placement"]["description"],
        format!(
            "{S060_PLACEMENT_DESCRIPTION_PREFIX} No child placements are available in this session."
        )
    );

    let escaped = s060_definition(&["😀", "z", "quote\"key", "α", "z"]);
    assert_eq!(
        escaped.input_schema["properties"]["placement"]["description"],
        format!(
            "{S060_PLACEMENT_DESCRIPTION_PREFIX} Available placements: \"quote\\\"key\", \"z\", \"α\", \"😀\"."
        ),
        "placement discovery uses Unicode scalar ordering and JSON quoting"
    );
}

#[test]
fn s060_a11_normative_descriptions_are_exact_and_guidance_only_replaces_tool_text() {
    let definition = s060_definition(&["workspace"]);
    assert_eq!(definition.description, S060_DEFAULT_SPAWN_DESCRIPTION);
    assert_eq!(
        definition.input_schema["properties"]["placement"]["description"],
        format!("{S060_PLACEMENT_DESCRIPTION_PREFIX} Available placements: \"workspace\".")
    );
    assert_eq!(
        definition.input_schema["properties"]["instructions"]["description"],
        S060_INSTRUCTIONS_DESCRIPTION
    );
    assert_eq!(
        definition.input_schema["properties"]["task"]["description"],
        S060_TASK_DESCRIPTION
    );
    assert_eq!(
        definition.input_schema["properties"]["budget"]["description"],
        S060_BUDGET_DESCRIPTION
    );
    assert_eq!(
        definition.input_schema["properties"]["capabilities"]["description"],
        S060_CAPABILITIES_DESCRIPTION
    );

    let (guided, _receiver) = s060_spawn_tool(
        &["workspace"],
        ResourceBudget::new(0, 0, Decimal::ZERO, 0),
        Some(SpawnAgentGuidance {
            description: "Embedding-authored lifecycle guidance.".into(),
            result_note: None,
        }),
    );
    let guided = guided.definition();
    assert_eq!(guided.description, "Embedding-authored lifecycle guidance.");
    assert_eq!(guided.input_schema, definition.input_schema);
}

#[test]
fn s060_a07_resolution_uses_child_placements_not_a_same_named_agent_type() {
    let with_placement = s060_parse_runtime_config(
        r#"
[project]
name = "s060-resolution"

[agent_types.workspace]
model = "root-model"

[child_placements.workspace]
model = "child-model"
max_tokens = 5
"#,
    );
    let request_at_placement_boundary =
        s060_supervisor_request("workspace", ResourceBudget::new(5, 1, Decimal::ZERO, 0));
    s060_real_task_factory(with_placement)
        .validate_spawn_config(&request_at_placement_boundary)
        .expect("the configured child placement should resolve");

    let stale_config = s060_parse_runtime_config(
        r#"
[project]
name = "s060-resolution"

[agent_types.workspace]
model = "root-model"
"#,
    );
    let error = s060_real_task_factory(stale_config)
        .validate_spawn_config(&request_at_placement_boundary)
        .expect_err("a same-named root agent must not substitute for a deleted placement")
        .to_string();
    assert!(
        error.contains("workspace"),
        "error names requested key: {error}"
    );
    assert!(
        error.contains("placement"),
        "error explains placement resolution: {error}"
    );
}

#[tokio::test]
async fn s060_a07_stale_tool_allow_list_cannot_bypass_live_placement_resolution() {
    let stale_config = s060_parse_runtime_config(
        r#"
[project]
name = "s060-stale-resolution"

[agent_types.workspace]
model = "root-model"
"#,
    );
    let provider_constructions = Arc::new(AtomicUsize::new(0));
    let provider_constructions_for_factory = Arc::clone(&provider_constructions);
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    let mut task_factory = s060_real_task_factory(stale_config);
    task_factory.supervisor_sender = Some(sender.clone());
    task_factory.child_provider_factory = Some(Arc::new(move |_kind, _model| {
        provider_constructions_for_factory.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(S060CatalogCapturingProvider {
            captured: Arc::new(Mutex::new(Vec::new())),
        }))
    }));

    let parent_budget = Arc::new(Mutex::new(ResourceBudget::new(100, 10, Decimal::ZERO, 2)));
    let before_budget = parent_budget.lock().expect("budget lock").clone();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        s060_capability(&["workspace"]),
        Arc::clone(&parent_budget),
        Arc::new(task_factory),
    );
    let journal_port: Arc<dyn JournalStorage> = journal.clone();
    supervisor.set_journal_storage(journal_port);
    supervisor.set_root_agent_id(AgentId("root".into()));
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });

    let tool = SpawnAgentTool {
        sender,
        allowed_placements: vec!["workspace".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        parent_budget: Arc::clone(&parent_budget),
        guidance: None,
    };
    let result = tool
        .call(
            serde_json::json!({
                "placement": "workspace",
                "task": "bounded task",
                "budget": s060_budget(1, 1, "0", 1)
            }),
            &s060_capability(&["workspace"]),
        )
        .await;
    let error = result
        .expect_err("deleted placement must fail synchronously before acknowledgement")
        .to_string();
    assert!(
        error.contains("workspace"),
        "error names stale key: {error}"
    );
    assert!(
        error.contains("placement"),
        "error names resolution domain: {error}"
    );
    assert_eq!(provider_constructions.load(Ordering::SeqCst), 0);
    assert!(
        journal
            .read_all(&AgentId("parent-agent".into()))
            .expect("journal read")
            .is_empty()
    );
    assert_eq!(
        serde_json::to_value(parent_budget.lock().expect("budget lock").clone())
            .expect("budget serializes"),
        serde_json::to_value(before_budget).expect("budget serializes")
    );
    actor.abort();
}

#[tokio::test]
async fn s060_slice1_e2e_config_token_tool_actor_resolution_reaches_recording_factory() {
    let config = s060_parse_runtime_config(
        r#"
[project]
name = "s060-slice1-e2e"

[agent_types.root]
model = "root-model"
allowed_child_placements = ["workspace", "missing"]

[child_placements.workspace]
model = "child-model"
max_tokens = 5
max_turns = 2
max_sub_agents = 1
"#,
    );
    let parent_capability = simulacra_config::build_capability_token(
        config.agent_types.get("root").expect("configured root"),
    );
    assert_eq!(
        parent_capability.spawn_placements,
        vec!["workspace", "missing"],
        "root authorization should reach the runtime token without role aliases"
    );

    let mut allowed_placements = parent_capability
        .spawn_placements
        .iter()
        .filter(|placement| config.child_placements.contains_key(*placement))
        .cloned()
        .collect::<Vec<_>>();
    allowed_placements.sort();
    allowed_placements.dedup();
    assert_eq!(allowed_placements, vec!["workspace"]);

    let recorder = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let validating_factory = S060ValidatingRecordingFactory {
        validator: s060_real_task_factory(config),
        recorder: recorder.clone(),
    };
    let parent_budget = Arc::new(Mutex::new(ResourceBudget::new(
        9,
        4,
        Decimal::ZERO,
        2,
    )));
    let journal = Arc::new(InMemoryJournalStorage::new());
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        parent_capability.clone(),
        Arc::clone(&parent_budget),
        Arc::new(validating_factory),
    );
    let journal_port: Arc<dyn JournalStorage> = journal.clone();
    supervisor.set_journal_storage(journal_port);
    supervisor.set_root_agent_id(AgentId("root".into()));
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });

    let tool = SpawnAgentTool {
        sender: sender.clone(),
        allowed_placements,
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("root".into()),
        parent_budget,
        guidance: None,
    };
    let acknowledgement = tool
        .call(
            serde_json::json!({
                "placement": "workspace",
                "instructions": "  preserve shaping bytes \n",
                "task": "  bounded delegated task \n",
                "budget": s060_budget(5, 2, "0", 1)
            }),
            &parent_capability,
        )
        .await
        .expect("configured and authorized placement should be acknowledged");
    assert_eq!(acknowledgement["placement"], "workspace");
    assert_eq!(acknowledgement["status"], "running");
    assert!(acknowledgement.get("agent_type").is_none());

    recorder.wait_for_completed(1).await;
    assert_eq!(recorder.started_count(), 1);
    {
        let started = recorder.inner.started.lock().expect("started snapshots");
        assert_eq!(started[0].placement, "workspace");
        assert_eq!(started[0].task, "  bounded delegated task \n");
        assert_eq!(started[0].max_tokens, 5);
        assert_eq!(started[0].max_turns, 2);
        assert_eq!(started[0].max_sub_agents, 1);
    }
    assert_eq!(
        journal
            .read_all(&AgentId("root".into()))
            .expect("parent journal")
            .len(),
        2,
        "accepted spawn should journal spawn and completion exactly once"
    );

    drop(tool);
    drop(sender);
    actor.await.expect("supervisor actor should drain cleanly");
}

#[test]
fn s060_a05_placement_descendant_authorization_reaches_the_spawned_child_capability() {
    let config = s060_parse_runtime_config(
        r#"
[project]
name = "s060-descendant-capability"

[agent_types.workspace]
model = "root-model"

[child_placements.workspace]
model = "child-model"
max_sub_agents = 1
allowed_child_placements = ["in_process"]

[child_placements.in_process]
model = "leaf-model"
"#,
    );
    let captured = Arc::new(Mutex::new(Vec::<ToolDefinition>::new()));
    let captured_for_provider = Arc::clone(&captured);
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let mut factory = s060_real_task_factory(config);
    factory.supervisor_sender = Some(sender);
    factory.child_provider_factory = Some(Arc::new(move |_kind, _model| {
        Ok(Box::new(S060CatalogCapturingProvider {
            captured: Arc::clone(&captured_for_provider),
        }))
    }));

    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime builds")
        .block_on(factory.create_task(
            s060_supervisor_request("workspace", ResourceBudget::new(10, 1, Decimal::ZERO, 1)),
            CancellationToken::new(Duration::from_secs(1)),
        ))
        .expect("configured native child should run");
    assert_eq!(result.exit_reason, ExitReason::Complete);

    let definitions = captured.lock().expect("catalog capture lock");
    let spawn = definitions
        .iter()
        .find(|definition| definition.name == "spawn_agent")
        .expect("placement descendant capability should register spawn_agent");
    let description = spawn.input_schema["properties"]["placement"]["description"]
        .as_str()
        .expect("spawn placement description");
    assert!(description.contains("\"in_process\""));
    assert!(!description.contains("\"workspace\""));
}

#[test]
fn s060_a18_real_placement_resolution_enforces_each_configured_maximum() {
    let dimensions = [
        (
            "max_tokens",
            "5",
            ResourceBudget::new(5, 1, Decimal::ZERO, 0),
            ResourceBudget::new(6, 1, Decimal::ZERO, 0),
            ResourceBudget::new(0, 1, Decimal::ZERO, 0),
        ),
        (
            "max_turns",
            "4",
            ResourceBudget::new(1, 4, Decimal::ZERO, 0),
            ResourceBudget::new(1, 5, Decimal::ZERO, 0),
            ResourceBudget::new(1, 0, Decimal::ZERO, 0),
        ),
        (
            "max_cost",
            "3.25",
            ResourceBudget::new(1, 1, Decimal::new(325, 2), 0),
            ResourceBudget::new(1, 1, Decimal::new(326, 2), 0),
            ResourceBudget::new(1, 1, Decimal::ZERO, 0),
        ),
        (
            "max_sub_agents",
            "2",
            ResourceBudget::new(1, 1, Decimal::ZERO, 2),
            ResourceBudget::new(1, 1, Decimal::ZERO, 3),
            ResourceBudget::new(1, 1, Decimal::ZERO, 0),
        ),
    ];

    for (field, limit, exact, over, zero) in dimensions {
        let assignment = if field == "max_cost" {
            format!("{field} = {limit:?}")
        } else {
            format!("{field} = {limit}")
        };
        let config = s060_parse_runtime_config(&format!(
            r#"
[project]
name = "s060-placement-budget"

[agent_types.workspace]
model = "root-model"

[child_placements.workspace]
model = "child-model"
{assignment}
"#
        ));
        let factory = s060_real_task_factory(config);
        factory
            .validate_spawn_config(&s060_supervisor_request("workspace", exact))
            .unwrap_or_else(|error| {
                panic!("exact {field} placement boundary should pass: {error}")
            });

        for (request, requested) in [(over, None), (zero, Some("0"))] {
            let requested = requested.map(str::to_owned).unwrap_or_else(|| {
                let value = serde_json::to_value(&request).expect("budget serializes");
                value[field]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value[field].to_string())
            });
            let error = factory
                .validate_spawn_config(&s060_supervisor_request("workspace", request))
                .expect_err("above-limit or unlimited request under finite placement must fail")
                .to_string();
            assert!(error.contains(field), "error should name {field}: {error}");
            assert!(
                error.contains(&requested),
                "error should name request: {error}"
            );
            assert!(
                error.contains(limit),
                "error should name limit {limit}: {error}"
            );
        }
    }

    for maximum in [None, Some("0")] {
        let assignment = maximum
            .map(|value| format!("max_tokens = {value}"))
            .unwrap_or_default();
        let config = s060_parse_runtime_config(&format!(
            r#"
[project]
name = "s060-unlimited-placement-budget"

[agent_types.workspace]
model = "root-model"

[child_placements.workspace]
model = "child-model"
{assignment}
"#
        ));
        s060_real_task_factory(config)
            .validate_spawn_config(&s060_supervisor_request(
                "workspace",
                ResourceBudget::new(0, 1, Decimal::ZERO, 0),
            ))
            .expect("absent or zero placement maximum is unlimited");
    }
}

#[tokio::test]
async fn s060_a12_unknown_and_legacy_top_level_arguments_are_rejected_before_dispatch() {
    for rejected in [
        "child_id",
        "agent_type",
        "system_prompt",
        "tier",
        "invented",
    ] {
        let (tool, receiver) = s060_spawn_tool(
            &["workspace"],
            ResourceBudget::new(0, 0, Decimal::ZERO, 0),
            None,
        );
        let mut arguments = serde_json::json!({
            "placement": "workspace",
            "task": "inspect the focused change",
            "budget": s060_budget(1, 1, "0", 0)
        });
        arguments
            .as_object_mut()
            .expect("arguments object")
            .insert(rejected.into(), serde_json::json!("legacy"));
        let (result, dispatched) =
            s060_call_and_capture(&tool, receiver, arguments, &s060_capability(&["workspace"]))
                .await;
        let error = result
            .expect_err("unknown top-level field should fail")
            .to_string();
        assert!(
            error.contains(rejected),
            "error should name {rejected}: {error}"
        );
        assert!(dispatched.is_none(), "invalid call reached the supervisor");
    }
}

#[tokio::test]
async fn s060_a13_unknown_nested_arguments_are_rejected_before_dispatch() {
    let cases = [
        ("budget", "invented"),
        ("capabilities", "invented"),
        ("capabilities", "spawn_types"),
    ];
    for (object, rejected) in cases {
        let (tool, receiver) = s060_spawn_tool(
            &["workspace"],
            ResourceBudget::new(0, 0, Decimal::ZERO, 0),
            None,
        );
        let mut arguments = serde_json::json!({
            "placement": "workspace",
            "task": "inspect the focused change",
            "budget": s060_budget(1, 1, "0", 0),
            "capabilities": {}
        });
        arguments[object][rejected] = serde_json::json!(true);
        let (result, dispatched) =
            s060_call_and_capture(&tool, receiver, arguments, &s060_capability(&["workspace"]))
                .await;
        let error = result
            .expect_err("unknown nested field should fail")
            .to_string();
        assert!(
            error.contains(rejected),
            "error should name {rejected}: {error}"
        );
        assert!(dispatched.is_none(), "invalid call reached the supervisor");
    }
}

#[tokio::test]
async fn s060_a08_a13_missing_budget_members_and_malformed_values_fail_before_dispatch() {
    let valid = || {
        serde_json::json!({
            "placement": "workspace",
            "task": "inspect the focused change",
            "budget": s060_budget(1, 1, "1.25", 1),
            "capabilities": {}
        })
    };

    let mut missing_budget = valid();
    missing_budget
        .as_object_mut()
        .expect("arguments object")
        .remove("budget");
    s060_expect_rejected_before_dispatch(missing_budget, "budget").await;

    for field in ["max_tokens", "max_turns", "max_cost", "max_sub_agents"] {
        let mut arguments = valid();
        arguments["budget"]
            .as_object_mut()
            .expect("budget object")
            .remove(field);
        s060_expect_rejected_before_dispatch(arguments, field).await;
    }

    let malformed = [
        ("max_tokens", serde_json::json!(-1)),
        ("max_tokens", serde_json::json!("1")),
        ("max_turns", serde_json::json!(-1)),
        ("max_turns", serde_json::json!(u64::from(u32::MAX) + 1)),
        ("max_cost", serde_json::json!(1.25)),
        ("max_cost", serde_json::json!("not-a-decimal")),
        ("max_cost", serde_json::json!("-0.01")),
        ("max_sub_agents", serde_json::json!(-1)),
        ("max_sub_agents", serde_json::json!(u64::from(u32::MAX) + 1)),
    ];
    for (field, value) in malformed {
        let mut arguments = valid();
        arguments["budget"][field] = value;
        s060_expect_rejected_before_dispatch(arguments, field).await;
    }

    for (field, value) in [
        ("network", serde_json::json!(true)),
        ("mcp_tools", serde_json::json!("mcp:github:*")),
        ("shell", serde_json::json!("true")),
        ("javascript", serde_json::json!(1)),
        ("python", serde_json::Value::Null),
        ("paths_write", serde_json::json!([1])),
        ("paths_read", serde_json::json!({})),
        ("spawn_placements", serde_json::json!([false])),
    ] {
        let mut arguments = valid();
        arguments["capabilities"][field] = value;
        s060_expect_rejected_before_dispatch(arguments, field).await;
    }
}

#[tokio::test]
async fn s060_a14_invalid_or_unauthorized_placement_is_rejected_before_dispatch() {
    let cases = [
        (
            serde_json::Value::Null,
            vec!["workspace"],
            vec!["workspace"],
            "placement",
        ),
        (
            serde_json::json!(""),
            vec!["workspace"],
            vec!["workspace"],
            "placement",
        ),
        (
            serde_json::json!("missing"),
            vec!["workspace"],
            vec!["workspace"],
            "missing",
        ),
        (
            serde_json::json!("workspace"),
            vec!["in_process"],
            vec!["workspace"],
            "workspace",
        ),
        (
            serde_json::json!("workspace"),
            vec!["workspace"],
            vec!["in_process"],
            "workspace",
        ),
    ];

    for (placement, host_allowed, caller_allowed, rejected) in cases {
        let mut original_budget = ResourceBudget::new(100, 10, Decimal::ZERO, 1);
        original_budget.used_tokens = 17;
        original_budget.used_turns = 2;
        let (tool, receiver, budget_handle) =
            s060_spawn_tool_with_budget_handle(&host_allowed, original_budget.clone(), None);
        let mut arguments = serde_json::json!({
            "task": "inspect the focused change",
            "budget": s060_budget(1, 1, "0", 0)
        });
        if !placement.is_null() {
            arguments["placement"] = placement;
        }
        let (result, dispatched) = s060_call_and_capture(
            &tool,
            receiver,
            arguments,
            &s060_capability(&caller_allowed),
        )
        .await;
        let error = result
            .expect_err("invalid placement should fail")
            .to_string();
        assert!(
            error.contains(rejected),
            "error should name {rejected}: {error}"
        );
        assert!(
            dispatched.is_none(),
            "denied placement reached the supervisor"
        );
        assert_eq!(
            serde_json::to_value(budget_handle.lock().expect("budget lock").clone())
                .expect("budget serializes"),
            serde_json::to_value(original_budget).expect("budget serializes"),
            "denied placement must leave all budget counters unchanged"
        );
    }

    let (tool, receiver) = s060_spawn_tool(
        &["zeta", "alpha", "βeta"],
        ResourceBudget::new(0, 0, Decimal::ZERO, 0),
        None,
    );
    let (result, dispatched) = s060_call_and_capture(
        &tool,
        receiver,
        serde_json::json!({
            "placement": "missing",
            "task": "bounded task",
            "budget": s060_budget(1, 1, "0", 0)
        }),
        &s060_capability(&["zeta", "alpha", "βeta"]),
    )
    .await;
    let error = result
        .expect_err("unknown placement should fail")
        .to_string();
    let alpha = error.find("alpha").expect("error names alpha");
    let zeta = error.find("zeta").expect("error names zeta");
    let beta = error.find("βeta").expect("error names beta");
    assert!(
        alpha < zeta && zeta < beta,
        "available keys must be sorted: {error}"
    );
    assert!(dispatched.is_none());
}

#[tokio::test]
async fn s060_a14_empty_tool_or_capability_placement_lists_are_deny_all() {
    for (host, caller) in [
        (Vec::<&str>::new(), vec!["workspace"]),
        (vec!["workspace"], Vec::<&str>::new()),
        (Vec::<&str>::new(), Vec::<&str>::new()),
    ] {
        let arguments = serde_json::json!({
            "placement": "workspace",
            "task": "bounded task",
            "budget": s060_budget(1, 1, "0", 0)
        });
        let original_budget = ResourceBudget::new(10, 2, Decimal::ZERO, 1);
        let (tool, receiver, budget_handle) =
            s060_spawn_tool_with_budget_handle(&host, original_budget.clone(), None);
        let (result, dispatched) =
            s060_call_and_capture(&tool, receiver, arguments, &s060_capability(&caller)).await;
        let error = result
            .expect_err("an empty authorization list must deny")
            .to_string();
        assert!(error.contains("workspace") || error.contains("placement"));
        assert!(dispatched.is_none());
        assert_eq!(
            serde_json::to_value(budget_handle.lock().expect("budget lock").clone())
                .expect("budget serializes"),
            serde_json::to_value(original_budget).expect("budget serializes")
        );
    }
}

#[tokio::test]
async fn s060_a14_supervisor_independently_denies_unauthorized_and_empty_authorization() {
    for authorized in [vec!["in_process"], Vec::<&str>::new()] {
        let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
        let mut original_budget = ResourceBudget::new(100, 10, Decimal::ZERO, 4);
        original_budget.used_tokens = 11;
        original_budget.used_turns = 2;
        original_budget.used_sub_agents = 1;
        let mut supervisor = AgentSupervisor::with_task_factory(
            s060_capability(&authorized),
            original_budget.clone(),
            Arc::new(factory.clone()),
        );
        let journal = Arc::new(InMemoryJournalStorage::new());
        let journal_port: Arc<dyn JournalStorage> = journal.clone();
        supervisor.set_journal_storage(journal_port);
        supervisor.set_root_agent_id(AgentId("parent-agent".into()));

        let error = supervisor
            .spawn_agent(s060_supervisor_request(
                "workspace",
                ResourceBudget::new(1, 1, Decimal::ZERO, 0),
            ))
            .expect_err("the supervisor boundary must deny unauthorized placement")
            .to_string();
        assert!(error.contains("workspace") || error.contains("placement"));
        assert_eq!(factory.started_count(), 0, "denial must not invoke factory");
        assert!(
            journal
                .read_all(&AgentId("parent-agent".into()))
                .expect("journal read")
                .is_empty(),
            "denial must not append SubAgentSpawned"
        );
        assert_eq!(
            serde_json::to_value(supervisor.parent_budget()).expect("budget serializes"),
            serde_json::to_value(&original_budget).expect("budget serializes"),
            "denial must preserve used_sub_agents and every other budget counter"
        );
    }
}

#[tokio::test]
async fn s060_a15_blank_instructions_become_none_and_nonblank_bytes_are_preserved() {
    for instructions in [
        None,
        Some(""),
        Some(" "),
        Some("\n\t"),
        Some("\u{2003}\u{3000}"),
    ] {
        let (tool, receiver) = s060_spawn_tool(
            &["workspace"],
            ResourceBudget::new(0, 0, Decimal::ZERO, 0),
            None,
        );
        let mut arguments = serde_json::json!({
            "placement": "workspace",
            "task": "bounded task",
            "budget": s060_budget(1, 1, "0", 0)
        });
        if let Some(instructions) = instructions {
            arguments["instructions"] = serde_json::json!(instructions);
        }
        let (result, dispatched) =
            s060_call_and_capture(&tool, receiver, arguments, &s060_capability(&["workspace"]))
                .await;
        result.expect("absent or blank instructions should be accepted");
        assert_eq!(
            s060_captured_instructions(&dispatched.expect("accepted call dispatches")),
            None,
            "blank instructions are semantically absent"
        );
    }

    let instructions = "  preserve shaping bytes exactly \n";
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
            "instructions": instructions,
            "task": "bounded task",
            "budget": s060_budget(1, 1, "0", 0)
        }),
        &s060_capability(&["workspace"]),
    )
    .await;
    result.expect("nonblank instructions should be accepted");
    assert_eq!(
        s060_captured_instructions(&dispatched.expect("accepted call dispatches")),
        Some(instructions)
    );
}

#[tokio::test]
async fn s060_a16_instruction_byte_limit_accepts_boundary_and_rejects_oversize() {
    for accepted in ["x".repeat(65_536), "é".repeat(32_768)] {
        assert_eq!(accepted.len(), 65_536);
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
                "instructions": accepted,
                "task": "bounded task",
                "budget": s060_budget(1, 1, "0", 0)
            }),
            &s060_capability(&["workspace"]),
        )
        .await;
        result.expect("65,536 UTF-8 instruction bytes should be accepted");
        assert!(dispatched.is_some());
    }

    for oversized in [
        "x".repeat(65_537),
        format!("{}x", "é".repeat(32_768)),
        format!("{}  ", "\u{3000}".repeat(21_845)),
    ] {
        assert_eq!(oversized.len(), 65_537);
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
                "instructions": oversized,
                "task": "bounded task",
                "budget": s060_budget(1, 1, "0", 0)
            }),
            &s060_capability(&["workspace"]),
        )
        .await;
        let error = result
            .expect_err("65,537 instruction bytes should fail")
            .to_string();
        assert!(error.contains("instructions"));
        assert!(error.contains("65537"));
        assert!(error.contains("65536"));
        assert!(dispatched.is_none());
    }
}

#[tokio::test]
async fn s060_a17_task_is_required_nonblank_and_preserved_byte_for_byte() {
    for task in [
        None,
        Some(""),
        Some(" "),
        Some("\n\t"),
        Some("\u{2003}\u{3000}"),
    ] {
        let (tool, receiver) = s060_spawn_tool(
            &["workspace"],
            ResourceBudget::new(0, 0, Decimal::ZERO, 0),
            None,
        );
        let mut arguments = serde_json::json!({
            "placement": "workspace",
            "budget": s060_budget(1, 1, "0", 0)
        });
        if let Some(task) = task {
            arguments["task"] = serde_json::json!(task);
        }
        let (result, dispatched) =
            s060_call_and_capture(&tool, receiver, arguments, &s060_capability(&["workspace"]))
                .await;
        let error = result
            .expect_err("missing or blank task should fail")
            .to_string();
        assert!(error.contains("task"), "error should name task: {error}");
        assert!(dispatched.is_none());
    }

    let task = "  preserve this task exactly \n";
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
            "task": task,
            "budget": s060_budget(1, 1, "0", 0)
        }),
        &s060_capability(&["workspace"]),
    )
    .await;
    result.expect("nonblank task should be accepted");
    let captured = dispatched.expect("valid call should dispatch");
    assert_eq!(captured.task.as_bytes(), task.as_bytes());
}

#[tokio::test]
async fn s060_a18_budget_requests_are_validated_not_inherited_or_clamped() {
    let dimensions = [
        (
            "max_tokens",
            serde_json::json!(5),
            serde_json::json!(6),
            "5",
        ),
        ("max_turns", serde_json::json!(4), serde_json::json!(5), "4"),
        (
            "max_cost",
            serde_json::json!("3.25"),
            serde_json::json!("3.26"),
            "3.25",
        ),
        (
            "max_sub_agents",
            serde_json::json!(2),
            serde_json::json!(3),
            "2",
        ),
    ];

    for (field, boundary, over, finite_limit) in dimensions {
        let (unlimited_tool, unlimited_receiver) = s060_spawn_tool(
            &["workspace"],
            ResourceBudget::new(0, 0, Decimal::ZERO, 0),
            None,
        );
        let (unlimited_result, unlimited_dispatch) = s060_call_and_capture(
            &unlimited_tool,
            unlimited_receiver,
            serde_json::json!({
                "placement": "workspace",
                "task": "bounded task",
                "budget": s060_budget(0, 0, "0", 0)
            }),
            &s060_capability(&["workspace"]),
        )
        .await;
        unlimited_result.unwrap_or_else(|error| {
            panic!("zero {field} request under an unlimited parent should pass: {error}")
        });
        assert!(unlimited_dispatch.is_some());

        let parent = ResourceBudget::new(
            if field == "max_tokens" { 5 } else { 0 },
            if field == "max_turns" { 4 } else { 0 },
            if field == "max_cost" {
                "3.25".parse().expect("decimal")
            } else {
                Decimal::ZERO
            },
            if field == "max_sub_agents" { 2 } else { 0 },
        );

        for (request, should_succeed) in [
            (serde_json::Value::from(0), false),
            (boundary.clone(), true),
            (over, false),
        ] {
            let (tool, receiver) = s060_spawn_tool(&["workspace"], parent.clone(), None);
            let mut budget = s060_budget(1, 1, "1", 1);
            budget[field] = if field == "max_cost" && request == serde_json::json!(0) {
                serde_json::json!("0")
            } else {
                request.clone()
            };
            let (result, dispatched) = s060_call_and_capture(
                &tool,
                receiver,
                serde_json::json!({
                    "placement": "workspace",
                    "task": "bounded task",
                    "budget": budget
                }),
                &s060_capability(&["workspace"]),
            )
            .await;

            if should_succeed {
                result.unwrap_or_else(|error| {
                    panic!("request at {field} boundary should pass: {error}")
                });
                let captured = dispatched.expect("accepted boundary request dispatches");
                let captured_budget =
                    serde_json::to_value(captured.budget).expect("captured budget serializes");
                assert_eq!(
                    captured_budget[field], budget[field],
                    "an accepted {field} request must not be clamped or replaced"
                );
            } else {
                let error = result
                    .expect_err("zero under finite parent or over-limit request should fail")
                    .to_string();
                assert!(error.contains(field), "error should name {field}: {error}");
                let requested = request
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| request.to_string());
                assert!(
                    error.contains(&requested),
                    "error should name requested {field} value {requested}: {error}"
                );
                assert!(
                    error.contains(finite_limit),
                    "error should name finite limit {finite_limit}: {error}"
                );
                assert!(dispatched.is_none());
            }
        }
    }
}

#[tokio::test]
async fn s060_a18_parent_remaining_budget_is_used_for_each_dimension() {
    let cases = [
        (
            "max_tokens",
            serde_json::json!(6),
            serde_json::json!(7),
            "6",
        ),
        ("max_turns", serde_json::json!(6), serde_json::json!(7), "6"),
        (
            "max_cost",
            serde_json::json!("6.00"),
            serde_json::json!("6.01"),
            "6.00",
        ),
        (
            "max_sub_agents",
            serde_json::json!(1),
            serde_json::json!(2),
            "1",
        ),
    ];

    for (field, exact, over, remaining) in cases {
        for (request, succeeds) in [(exact, true), (over, false)] {
            let mut parent = ResourceBudget::new(10, 10, Decimal::new(1000, 2), 3);
            parent.used_tokens = 4;
            parent.used_turns = 4;
            parent.used_cost = Decimal::new(400, 2);
            parent.used_sub_agents = 2;
            let (tool, receiver) = s060_spawn_tool(&["workspace"], parent, None);
            let mut budget = s060_budget(1, 1, "1", 1);
            budget[field] = request.clone();
            let (result, dispatched) = s060_call_and_capture(
                &tool,
                receiver,
                serde_json::json!({
                    "placement": "workspace",
                    "task": "bounded task",
                    "budget": budget
                }),
                &s060_capability(&["workspace"]),
            )
            .await;

            if succeeds {
                result.unwrap_or_else(|error| {
                    panic!("request at remaining {field} boundary should pass: {error}")
                });
                let captured = dispatched.expect("accepted request dispatches");
                assert_eq!(
                    serde_json::to_value(captured.budget).expect("budget serializes")[field],
                    budget[field]
                );
            } else {
                let error = result
                    .expect_err("request beyond parent remaining must fail")
                    .to_string();
                assert!(error.contains(field), "error should name {field}: {error}");
                assert!(
                    error.contains(remaining),
                    "error should name remaining limit {remaining}: {error}"
                );
                let requested = request
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| request.to_string());
                assert!(
                    error.contains(&requested),
                    "error should name request: {error}"
                );
                assert!(dispatched.is_none());
            }
        }
    }
}
