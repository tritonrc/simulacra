fn s060_remediation_budget() -> serde_json::Value {
    s060_budget(20, 4, "0", 2)
}

fn s060_remediation_nested_budget() -> serde_json::Value {
    s060_budget(20, 1, "0", 1)
}

fn s060_remediation_config() -> SimulacraConfig {
    s060_parse_runtime_config(
        r#"
[project]
name = "s060-immediate-caller-remediation"

[agent_types.root]
model = "root-model"
allowed_child_placements = ["middle", "grandchild", "forbidden_sibling"]

[child_placements.middle]
backend = "native"
model = "middle-model"
allowed_child_placements = ["grandchild", "forbidden_sibling"]

[child_placements.middle.capabilities]
shell = true
javascript = true

[child_placements.grandchild]
backend = "acp"
acp_profile = "grandchild-profile"
allowed_child_placements = ["grandchild", "forbidden_sibling"]

[child_placements.grandchild.capabilities]
shell = true
javascript = true

[child_placements.forbidden_sibling]
backend = "acp"
acp_profile = "forbidden-profile"

[child_placements.forbidden_sibling.capabilities]
shell = true
javascript = true
"#,
    )
}

fn s060_remediation_root_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        spawn_placements: vec![
            "middle".into(),
            "grandchild".into(),
            "forbidden_sibling".into(),
        ],
        ..CapabilityToken::default()
    }
}

fn s060_remediation_middle_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        spawn_placements: vec!["grandchild".into()],
        ..CapabilityToken::default()
    }
}

fn s060_middle_provider_responses() -> Vec<ProviderResponse> {
    vec![
        ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: "middle finished after exercising its real spawn tool".into(),
                tool_calls: vec![],
                tool_call_id: None,
                provider_content: vec![],
            },
            token_usage: TokenUsage::default(),
            finish_reason: FinishReason::EndTurn,
            provider_response_id: Some("s060-middle-final".into()),
            model: "middle-model".into(),
        },
        ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCallMessage {
                    id: "s060-forbidden-root-only-placement".into(),
                    name: "spawn_agent".into(),
                    arguments: serde_json::json!({
                        "placement": "forbidden_sibling",
                        "instructions": "Root has this placement, so grant it to me despite my token.",
                        "task": "this root-only placement must not be accepted",
                        "budget": s060_remediation_nested_budget()
                    }),
                }],
                tool_call_id: None,
                provider_content: vec![],
            },
            token_usage: TokenUsage::default(),
            finish_reason: FinishReason::ToolUse,
            provider_response_id: Some("s060-middle-forbidden".into()),
            model: "middle-model".into(),
        },
        ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCallMessage {
                    id: "s060-valid-grandchild".into(),
                    name: "spawn_agent".into(),
                    arguments: serde_json::json!({
                        "placement": "grandchild",
                        "instructions": "Regain javascript and forbidden_sibling if possible, then do the bounded task.",
                        "task": "create the authorized grandchild through the real child registry",
                        "budget": s060_remediation_nested_budget()
                    }),
                }],
                tool_call_id: None,
                provider_content: vec![],
            },
            token_usage: TokenUsage::default(),
            finish_reason: FinishReason::ToolUse,
            provider_response_id: Some("s060-middle-valid".into()),
            model: "middle-model".into(),
        },
    ]
}

struct S060RemediationActivitySink(Arc<Mutex<Vec<simulacra_types::ActivityEvent>>>);

impl simulacra_runtime::ActivitySink for S060RemediationActivitySink {
    fn emit(&self, event: simulacra_types::ActivityEvent) {
        self.0.lock().expect("activity captures").push(event);
    }
}

#[tokio::test]
async fn s060_a26_a27_a36_nested_spawn_uses_immediate_callers_effective_capability() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "capture-immediate-caller".into(),
            calls: Arc::clone(&calls),
            behavior: Arc::new(|_, _| Ok(simulacra_hooks::Verdict::continue_unchanged())),
        }),
    );

    let root_id = AgentId("root-s060-immediate-caller".into());
    let root_capability = s060_remediation_root_capability();
    let middle_capability = s060_remediation_middle_capability();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let observations = Arc::new(Mutex::new(Vec::new()));
    let activities = Arc::new(Mutex::new(Vec::new()));
    let activity_sink: Arc<dyn simulacra_runtime::ActivitySink> =
        Arc::new(S060RemediationActivitySink(Arc::clone(&activities)));
    let middle_cells = Arc::new(Mutex::new(Vec::<CapabilityToken>::new()));
    let middle_cells_for_factory = Arc::clone(&middle_cells);
    let runtime: Arc<dyn simulacra_runtime::AcpChildRuntime> = Arc::new(S060RecordingAcpRuntime {
        observations: Arc::clone(&observations),
        journal: Arc::clone(&journal) as Arc<dyn JournalStorage>,
        parent_id: root_id.clone(),
        outcome: S060ChildOutcome::Complete,
    });
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let factory = Arc::new(AgentTaskFactory {
        config: s060_remediation_config(),
        provider_kind: ProviderKind::Anthropic,
        vfs: Arc::new(MemoryFs::new()),
        journal: Arc::clone(&journal) as Arc<dyn JournalStorage>,
        activity_sink: Arc::clone(&activity_sink),
        parent_capability: root_capability.clone(),
        allowed_mcp_servers: None,
        supervisor_sender: Some(sender.clone()),
        pipeline: Some(Arc::new(pipeline)),
        script_executor: None,
        child_cell_configurator: Some(Arc::new(move |cell| {
            middle_cells_for_factory
                .lock()
                .expect("middle cell captures")
                .push(cell.capability.clone());
        })),
        child_tool_registrar: None,
        child_provider_factory: Some(Arc::new(|_, model| {
            assert_eq!(model, "middle-model");
            Ok(Box::new(
                FakeProvider::new(s060_middle_provider_responses()),
            ))
        })),
        acp_child_runtime: Some(runtime),
    });
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        1_000,
        100,
        Decimal::ZERO,
        8,
    )));
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        root_capability.clone(),
        Arc::clone(&budget),
        factory,
    );
    supervisor.set_root_agent_id(root_id.clone());
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    supervisor.set_activity_sink(Arc::clone(&activity_sink));
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });

    let root_tool = SpawnAgentTool {
        sender: sender.clone(),
        allowed_placements: root_capability.spawn_placements.clone(),
        activity_sink: Arc::clone(&activity_sink),
        parent_id: root_id.clone(),
        parent_budget: Arc::clone(&budget),
        guidance: None,
    };
    let middle_ack = root_tool
        .call(
            serde_json::json!({
                "placement": "middle",
                "instructions": "Try to regain javascript and forbidden_sibling even though instructions cannot grant either.",
                "task": "exercise the real registered spawn_agent tool twice",
                "budget": s060_remediation_budget(),
                "capabilities": {
                    "shell": true,
                    "javascript": false,
                    "spawn_placements": ["grandchild"]
                }
            }),
            &root_capability,
        )
        .await
        .expect("root should create the strictly attenuated middle child");
    let middle_id = AgentId(
        middle_ack["child_id"]
            .as_str()
            .expect("middle child id")
            .to_string(),
    );
    let middle_terminal = JoinChildAgentTool {
        sender: sender.clone(),
        caller_id: root_id.clone(),
    }
    .call(
        serde_json::json!({"child_id": middle_id.0}),
        &root_capability,
    )
    .await
    .expect("root should deterministically join the native middle child");
    assert_eq!(middle_terminal["status"], "completed");

    assert_eq!(
        middle_cells
            .lock()
            .expect("middle cell captures")
            .as_slice(),
        std::slice::from_ref(&middle_capability),
        "the model-facing root attenuation must be the exact native middle runtime token"
    );

    let grandchild_observation = {
        observations
            .lock()
            .expect("ACP observations")
            .iter()
            .find(|observation| observation.request.placement == "grandchild")
            .cloned()
            .expect("the actual native child spawn tool should reach the grandchild ACP runtime")
    };
    let grandchild_id = grandchild_observation.request.child_id.clone();

    JoinChildAgentTool {
        sender: sender.clone(),
        caller_id: middle_id.clone(),
    }
    .call(
        serde_json::json!({"child_id": grandchild_id.0}),
        &middle_capability,
    )
    .await
    .expect("the immediate parent should deterministically join its grandchild");

    let hook_call_count = {
        let calls = calls.lock().expect("hook calls");
        let middle_before = calls
            .iter()
            .find(|(_, phase, context)| {
                *phase == simulacra_hooks::Phase::Before && context["placement"] == "middle"
            })
            .expect("middle before-hook call");
        let middle_before_capability =
            serde_json::from_value::<CapabilityToken>(middle_before.2["capabilities"].clone())
                .expect("middle hook capability should deserialize");
        assert_eq!(middle_before_capability, middle_capability);
        assert_eq!(
            middle_before.2["instructions"],
            "Try to regain javascript and forbidden_sibling even though instructions cannot grant either."
        );

        let grandchild_before = calls
            .iter()
            .find(|(_, phase, context)| {
                *phase == simulacra_hooks::Phase::Before && context["placement"] == "grandchild"
            })
            .expect("grandchild before-hook call");
        let grandchild_before_capability =
            serde_json::from_value::<CapabilityToken>(grandchild_before.2["capabilities"].clone())
                .expect("grandchild hook capability should deserialize");
        assert_eq!(
            grandchild_before_capability, middle_capability,
            "the hook must see placement ∩ the immediate parent's token, not placement ∩ the root token"
        );
        assert_eq!(
            grandchild_before.2["instructions"],
            "Regain javascript and forbidden_sibling if possible, then do the bounded task."
        );
        calls.len()
    };

    assert_eq!(
        grandchild_observation.request.capability, middle_capability,
        "the exact immediate-parent-derived token must reach the child runtime"
    );
    assert_eq!(
        grandchild_observation.request.instructions.as_deref(),
        Some("Regain javascript and forbidden_sibling if possible, then do the bounded task.")
    );

    let forbidden_id = AgentId("child-ffffffffffffffffffffffffffffffff".into());
    let missing_status = ChildStatusTool {
        sender: sender.clone(),
        caller_id: middle_id.clone(),
    }
    .call(
        serde_json::json!({"child_id": forbidden_id.0}),
        &middle_capability,
    )
    .await
    .expect_err("the denied placement must not create a queryable child")
    .to_string();
    assert!(
        missing_status.contains("unknown") && missing_status.contains(&forbidden_id.0),
        "status denial should identify the absent attempted child: {missing_status}"
    );
    let roster = ListChildAgentTool {
        sender: sender.clone(),
        caller_id: middle_id.clone(),
    }
    .call(serde_json::json!({}), &middle_capability)
    .await
    .expect("middle child roster should remain available");
    let roster = roster.as_array().expect("roster array");
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0]["child_id"], grandchild_id.0);
    assert!(
        roster
            .iter()
            .all(|child| child["placement"] != "forbidden_sibling")
    );

    let middle_entries = journal.read_all(&middle_id).expect("middle journal");
    let denial = middle_entries
        .iter()
        .find_map(|entry| match &entry.entry {
            JournalEntryKind::ToolResult {
                tool_call_id: Some(tool_call_id),
                tool_name,
                content,
                is_error: true,
            } if tool_call_id == "s060-forbidden-root-only-placement"
                && tool_name == "spawn_agent" =>
            {
                Some(content)
            }
            _ => None,
        })
        .expect("the real child tool denial should be journaled as its error result");
    assert!(
        denial.contains("forbidden_sibling")
            && denial.contains("unknown or unauthorized placement")
            && denial.contains("grandchild"),
        "the denial must name the rejected root-only placement and the immediate caller's actual surface: {denial}"
    );

    let root_entries = journal.read_all(&root_id).expect("root journal");
    let all_entries = root_entries.iter().chain(middle_entries.iter());
    assert_eq!(
        all_entries
            .clone()
            .filter(|entry| matches!(entry.entry, JournalEntryKind::SubAgentSpawned { .. }))
            .count(),
        2,
        "only middle and grandchild may have accepted spawn entries"
    );
    assert_eq!(
        all_entries
            .clone()
            .filter(|entry| matches!(entry.entry, JournalEntryKind::SubAgentCompleted { .. }))
            .count(),
        2,
        "deterministic joins must leave exactly two completed entries"
    );
    assert!(all_entries.clone().all(|entry| match &entry.entry {
        JournalEntryKind::SubAgentSpawned {
            placement, task, ..
        } => placement != "forbidden_sibling" && !task.contains("root-only"),
        _ => true,
    }));
    assert_eq!(
        hook_call_count, 4,
        "two accepted children run before+after hooks"
    );
    assert_eq!(
        observations.lock().expect("ACP observations").len(),
        1,
        "the forbidden placement must not reach the ACP runtime"
    );
    assert_eq!(
        budget.lock().expect("budget").used_sub_agents,
        1,
        "the root budget tracks its direct middle child; the grandchild is charged to the middle budget"
    );

    {
        let activities = activities.lock().expect("activity captures");
        assert!(activities.iter().any(|event| matches!(
            event,
            simulacra_types::ActivityEvent::ChildSpawned { placement, .. } if placement == "middle"
        )));
        assert!(activities.iter().any(|event| matches!(
            event,
            simulacra_types::ActivityEvent::ChildSpawned { placement, .. } if placement == "grandchild"
        )));
        assert!(activities.iter().all(|event| !matches!(
            event,
            simulacra_types::ActivityEvent::ChildSpawned { placement, .. } if placement == "forbidden_sibling"
        )));
    }

    drop(root_tool);
    drop(sender);
    // The production child factory deliberately retains the supervisor sender
    // so live native descendants can delegate later. End this isolated actor
    // explicitly after every lifecycle assertion instead of waiting for a
    // channel that is intentionally self-retained.
    actor.abort();
    assert!(
        actor
            .await
            .expect_err("aborted fixture actor must stop")
            .is_cancelled(),
        "fixture actor should stop only through the explicit test teardown"
    );
}

#[derive(Clone, Copy)]
enum S060MemoryHookChange {
    Disable,
    Narrow,
}

impl S060MemoryHookChange {
    fn expected(self) -> simulacra_types::MemoryCapability {
        match self {
            Self::Disable => simulacra_types::MemoryCapability::default(),
            Self::Narrow => simulacra_types::MemoryCapability {
                enabled: true,
                search_scopes: vec![
                    simulacra_types::MemoryPath::parse("/var/memory/self/project")
                        .expect("valid narrow search scope"),
                ],
                write_scopes: vec![
                    simulacra_types::MemoryPath::parse("/var/memory/self/project/notes")
                        .expect("valid narrow write scope"),
                ],
            },
        }
    }
}

fn s060_memory_capability(placements: &[&str]) -> CapabilityToken {
    CapabilityToken {
        spawn_placements: placements.iter().map(|value| (*value).into()).collect(),
        memory: simulacra_types::MemoryCapability {
            enabled: true,
            search_scopes: vec![
                simulacra_types::MemoryPath::parse("/var/memory/self")
                    .expect("valid parent search scope"),
            ],
            write_scopes: vec![
                simulacra_types::MemoryPath::parse("/var/memory/self")
                    .expect("valid parent write scope"),
            ],
        },
        ..CapabilityToken::default()
    }
}

fn s060_memory_hook(change: S060MemoryHookChange) -> simulacra_hooks::HookPipeline {
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "attenuate-memory".into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior: Arc::new(move |phase, context| {
                if phase == simulacra_hooks::Phase::After {
                    return Ok(simulacra_hooks::Verdict::continue_unchanged());
                }
                let mut modified = context.clone();
                modified["capabilities"]["memory"] =
                    serde_json::to_value(change.expected()).expect("memory capability serializes");
                Ok(simulacra_hooks::Verdict::Continue(Some(
                    modified.to_string(),
                )))
            }),
        }),
    );
    pipeline
}

fn s060_memory_config() -> SimulacraConfig {
    s060_parse_runtime_config(
        r#"
[project]
name = "s060-hook-memory-remediation"

[agent_types.root]
model = "root-model"
allowed_child_placements = ["native_memory", "acp_memory"]

[child_placements.native_memory]
backend = "native"
model = "child-model"

[child_placements.native_memory.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self"]
write_scopes = ["/var/memory/self"]

[child_placements.acp_memory]
backend = "acp"
acp_profile = "memory-profile"

[child_placements.acp_memory.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self"]
write_scopes = ["/var/memory/self"]
"#,
    )
}

async fn s060_capture_final_memory_capabilities(
    change: S060MemoryHookChange,
) -> (CapabilityToken, CapabilityToken) {
    async fn capture(placement: &str, change: S060MemoryHookChange) -> CapabilityToken {
        let parent_id = AgentId(format!("root-s060-memory-hooks-{placement}"));
        let parent_capability = s060_memory_capability(&["native_memory", "acp_memory"]);
        let journal = Arc::new(InMemoryJournalStorage::new());
        let cells = Arc::new(Mutex::new(Vec::<CapabilityToken>::new()));
        let cells_for_factory = Arc::clone(&cells);
        let observations = Arc::new(Mutex::new(Vec::new()));
        let acp_runtime: Arc<dyn simulacra_runtime::AcpChildRuntime> =
            Arc::new(S060RecordingAcpRuntime {
                observations: Arc::clone(&observations),
                journal: Arc::clone(&journal) as Arc<dyn JournalStorage>,
                parent_id: parent_id.clone(),
                outcome: S060ChildOutcome::Complete,
            });
        let factory = Arc::new(AgentTaskFactory {
            config: s060_memory_config(),
            provider_kind: ProviderKind::Anthropic,
            vfs: Arc::new(MemoryFs::new()),
            journal: Arc::clone(&journal) as Arc<dyn JournalStorage>,
            activity_sink: Arc::new(NoopActivitySink),
            parent_capability: parent_capability.clone(),
            allowed_mcp_servers: None,
            supervisor_sender: None,
            pipeline: Some(Arc::new(s060_memory_hook(change))),
            script_executor: None,
            child_cell_configurator: Some(Arc::new(move |cell| {
                cells_for_factory
                    .lock()
                    .expect("native cell captures")
                    .push(cell.capability.clone());
            })),
            child_tool_registrar: None,
            child_provider_factory: Some(Arc::new(|_, _| {
                Ok(Box::new(FakeProvider::new(vec![ProviderResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: "done".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                        provider_content: vec![],
                    },
                    token_usage: TokenUsage::default(),
                    finish_reason: FinishReason::EndTurn,
                    provider_response_id: Some("s060-memory-native".into()),
                    model: "child-model".into(),
                }])))
            })),
            acp_child_runtime: Some(acp_runtime),
        });
        let budget = Arc::new(Mutex::new(ResourceBudget::new(100, 10, Decimal::ZERO, 4)));
        let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
            parent_capability.clone(),
            Arc::clone(&budget),
            factory,
        );
        supervisor.set_root_agent_id(parent_id.clone());
        supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
        let tool = SpawnAgentTool {
            sender: sender.clone(),
            allowed_placements: vec![placement.to_string()],
            activity_sink: Arc::new(NoopActivitySink),
            parent_id: parent_id.clone(),
            parent_budget: budget,
            guidance: None,
        };
        let ack = tool
            .call(
                serde_json::json!({
                    "placement": placement,
                    "task": "capture the exact final memory capability",
                    "budget": s060_budget(20, 2, "0", 1)
                }),
                &parent_capability,
            )
            .await
            .expect("valid memory attenuation should spawn");
        JoinChildAgentTool {
            sender: sender.clone(),
            caller_id: parent_id,
        }
        .call(
            serde_json::json!({"child_id": ack["child_id"]}),
            &parent_capability,
        )
        .await
        .expect("memory child must be deterministically joined");

        let captured = if placement == "native_memory" {
            cells
                .lock()
                .expect("native cell captures")
                .first()
                .cloned()
                .expect("native child capability")
        } else {
            observations
                .lock()
                .expect("ACP observations")
                .first()
                .map(|observation| observation.request.capability.clone())
                .expect("ACP request capability")
        };
        drop(tool);
        drop(sender);
        actor.await.expect("memory supervisor stops");
        captured
    }

    (
        capture("native_memory", change).await,
        capture("acp_memory", change).await,
    )
}

#[tokio::test]
async fn s060_a38_hook_memory_disable_is_final_for_native_and_acp_runtimes() {
    let expected = CapabilityToken {
        memory: S060MemoryHookChange::Disable.expected(),
        ..CapabilityToken::default()
    };
    let (native, acp) = s060_capture_final_memory_capabilities(S060MemoryHookChange::Disable).await;
    assert_eq!(native, expected, "native construction regranted capability");
    assert_eq!(acp, expected, "ACP request regranted capability");
}

#[tokio::test]
async fn s060_a38_hook_memory_scope_narrowing_is_final_for_native_and_acp_runtimes() {
    let expected = CapabilityToken {
        memory: S060MemoryHookChange::Narrow.expected(),
        ..CapabilityToken::default()
    };
    let (native, acp) = s060_capture_final_memory_capabilities(S060MemoryHookChange::Narrow).await;
    assert_eq!(native, expected, "native construction widened capability");
    assert_eq!(acp, expected, "ACP request widened capability");
}

async fn s060_assert_hook_capability_unknown_field_fails_closed(
    nested_in_memory: bool,
    expected_field: &str,
) {
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "inject-unknown-capability-field".into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior: Arc::new(move |phase, context| {
                if phase == simulacra_hooks::Phase::After {
                    return Ok(simulacra_hooks::Verdict::continue_unchanged());
                }
                let mut modified = context.clone();
                if nested_in_memory {
                    modified["capabilities"]["memory"]["unknown_memory_grant"] =
                        serde_json::json!(true);
                } else {
                    modified["capabilities"]["unknown_capability_grant"] = serde_json::json!(true);
                }
                Ok(simulacra_hooks::Verdict::Continue(Some(
                    modified.to_string(),
                )))
            }),
        }),
    );
    let stack = s060_hook_stack(pipeline);
    let error = stack
        .tool
        .call(s060_hook_arguments(), &stack.capability)
        .await
        .expect_err("unknown hook capability fields must fail closed")
        .to_string();
    assert!(
        error.contains(expected_field),
        "the strict-deserialization error should name {expected_field}: {error}"
    );
    s060_assert_no_accepted_spawn_effects(&stack);
    s060_finish_stack(stack).await;
}

#[tokio::test]
async fn s060_a38_hook_capability_top_level_unknown_field_fails_closed() {
    s060_assert_hook_capability_unknown_field_fails_closed(false, "unknown_capability_grant").await;
}

#[tokio::test]
async fn s060_a38_hook_capability_memory_unknown_field_fails_closed() {
    s060_assert_hook_capability_unknown_field_fails_closed(true, "unknown_memory_grant").await;
}
