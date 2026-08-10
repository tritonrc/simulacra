type S060HookBehavior = Arc<
    dyn Fn(
            simulacra_hooks::Phase,
            &serde_json::Value,
        ) -> Result<simulacra_hooks::Verdict, simulacra_hooks::HookError>
        + Send
        + Sync,
>;

struct S060RecordingHook {
    name: String,
    calls: Arc<Mutex<Vec<(String, simulacra_hooks::Phase, serde_json::Value)>>>,
    behavior: S060HookBehavior,
}

impl simulacra_hooks::HookModule for S060RecordingHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke(
        &self,
        phase: simulacra_hooks::Phase,
        operation: simulacra_hooks::Operation,
        context: &str,
    ) -> Result<simulacra_hooks::Verdict, simulacra_hooks::HookError> {
        assert_eq!(operation, simulacra_hooks::Operation::Spawn);
        let context: serde_json::Value =
            serde_json::from_str(context).expect("spawn hook context should be JSON");
        self.calls.lock().expect("hook call lock").push((
            self.name.clone(),
            phase,
            context.clone(),
        ));
        (self.behavior)(phase, &context)
    }
}

#[derive(Clone, Debug)]
struct S060RuntimeObservation {
    request: simulacra_runtime::AcpChildRequest,
    spawned_was_durable: bool,
}

struct S060RecordingAcpRuntime {
    observations: Arc<Mutex<Vec<S060RuntimeObservation>>>,
    journal: Arc<dyn JournalStorage>,
    parent_id: AgentId,
    outcome: S060ChildOutcome,
}

#[derive(Clone)]
enum S060ChildOutcome {
    Complete,
    MaxTurns,
    BudgetExhausted,
    ErrorExit,
    GuardrailTripped,
    Cancelled,
    PolicyKill,
    AwaitingApproval,
    CompleteAfter(Arc<tokio::sync::Barrier>),
}

impl simulacra_runtime::AcpChildRuntime for S060RecordingAcpRuntime {
    fn start_child(
        &self,
        request: simulacra_runtime::AcpChildRequest,
        _cancellation: CancellationToken,
        _activity_sink: Arc<dyn simulacra_runtime::ActivitySink>,
        _input_queue: simulacra_runtime::AgentInputQueue,
    ) -> simulacra_runtime::AcpChildFuture {
        let spawned_was_durable = self
            .journal
            .read_all(&self.parent_id)
            .expect("parent journal should be readable when ACP execution starts")
            .iter()
            .any(|entry| {
                serde_json::to_value(&entry.entry)
                    .ok()
                    .is_some_and(|entry| entry["type"] == "SubAgentSpawned")
            });
        self.observations
            .lock()
            .expect("runtime observation lock")
            .push(S060RuntimeObservation {
                request,
                spawned_was_durable,
            });
        let outcome = self.outcome.clone();
        Box::pin(async move {
            match outcome {
                S060ChildOutcome::Complete => Ok(child_success_output()),
                S060ChildOutcome::MaxTurns => Ok(AgentLoopOutput {
                    exit_reason: ExitReason::MaxTurns,
                    messages: vec![Message {
                        role: Role::Assistant,
                        content: "child max-turns result".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                        provider_content: vec![],
                    }],
                    token_usage: TokenUsage {
                        input_tokens: 3,
                        output_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    reported_tool_uses: None,
                    used_turns: 1,
                    used_cost: Decimal::new(15, 2),
                }),
                S060ChildOutcome::BudgetExhausted => Ok(AgentLoopOutput {
                    exit_reason: ExitReason::BudgetExhausted,
                    messages: vec![Message {
                        role: Role::Assistant,
                        content: "child budget-exhausted result".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                        provider_content: vec![],
                    }],
                    token_usage: TokenUsage {
                        input_tokens: 3,
                        output_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    reported_tool_uses: None,
                    used_turns: 1,
                    used_cost: Decimal::new(15, 2),
                }),
                S060ChildOutcome::ErrorExit => Ok(AgentLoopOutput {
                    exit_reason: ExitReason::Error("child provider failed".into()),
                    messages: vec![Message {
                        role: Role::Assistant,
                        content: "partial child failure result".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                        provider_content: vec![],
                    }],
                    token_usage: TokenUsage {
                        input_tokens: 3,
                        output_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    reported_tool_uses: None,
                    used_turns: 1,
                    used_cost: Decimal::new(15, 2),
                }),
                S060ChildOutcome::GuardrailTripped => Ok(AgentLoopOutput {
                    exit_reason: ExitReason::GuardrailTripped("child guardrail".into()),
                    messages: vec![Message {
                        role: Role::Assistant,
                        content: "child guardrail result".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                        provider_content: vec![],
                    }],
                    token_usage: TokenUsage {
                        input_tokens: 3,
                        output_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    reported_tool_uses: None,
                    used_turns: 1,
                    used_cost: Decimal::new(15, 2),
                }),
                S060ChildOutcome::Cancelled => Ok(AgentLoopOutput {
                    exit_reason: ExitReason::Cancelled,
                    messages: vec![Message {
                        role: Role::Assistant,
                        content: "child cancellation result".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                        provider_content: vec![],
                    }],
                    token_usage: TokenUsage {
                        input_tokens: 3,
                        output_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    reported_tool_uses: None,
                    used_turns: 1,
                    used_cost: Decimal::new(15, 2),
                }),
                S060ChildOutcome::PolicyKill => Ok(AgentLoopOutput {
                    exit_reason: ExitReason::PolicyKill {
                        hook: "child-policy".into(),
                        reason: "child policy terminated execution".into(),
                    },
                    messages: vec![Message {
                        role: Role::Assistant,
                        content: "child policy kill result".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                        provider_content: vec![],
                    }],
                    token_usage: TokenUsage {
                        input_tokens: 3,
                        output_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    reported_tool_uses: None,
                    used_turns: 1,
                    used_cost: Decimal::new(15, 2),
                }),
                S060ChildOutcome::AwaitingApproval => Ok(AgentLoopOutput {
                    exit_reason: ExitReason::AwaitingApproval,
                    messages: vec![Message {
                        role: Role::Assistant,
                        content: "child awaits approval".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                        provider_content: vec![],
                    }],
                    token_usage: TokenUsage {
                        input_tokens: 3,
                        output_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    reported_tool_uses: None,
                    used_turns: 1,
                    used_cost: Decimal::new(15, 2),
                }),
                S060ChildOutcome::CompleteAfter(release) => {
                    release.wait().await;
                    Ok(child_success_output())
                }
            }
        })
    }
}

struct S060HookStack {
    tool: SpawnAgentTool,
    join: JoinChildAgentTool,
    capability: CapabilityToken,
    journal: Arc<InMemoryJournalStorage>,
    budget: Arc<Mutex<ResourceBudget>>,
    observations: Arc<Mutex<Vec<S060RuntimeObservation>>>,
    actor: tokio::task::JoinHandle<()>,
    sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

fn s060_hook_config() -> SimulacraConfig {
    let config: SimulacraConfig = toml::from_str(
        r#"
[project]
name = "s060-hooks"

[agent_types.root]
model = "parent-model"
allowed_child_placements = ["workspace"]

[child_placements.workspace]
backend = "acp"
acp_profile = "workspace-pod"
allowed_child_placements = ["workspace"]

[child_placements.workspace.capabilities]
shell = true
"#,
    )
    .expect("S060 child placement config should parse");
    config
        .validate()
        .expect("S060 child placement config should validate");
    config
}

fn s060_hook_stack(pipeline: simulacra_hooks::HookPipeline) -> S060HookStack {
    s060_hook_stack_with_config(pipeline, s060_hook_config())
}

fn s060_hook_stack_with_outcome(
    pipeline: simulacra_hooks::HookPipeline,
    outcome: S060ChildOutcome,
) -> S060HookStack {
    s060_hook_stack_with_config_and_outcome(pipeline, s060_hook_config(), outcome)
}

fn s060_hook_stack_with_config(
    pipeline: simulacra_hooks::HookPipeline,
    config: SimulacraConfig,
) -> S060HookStack {
    s060_hook_stack_with_config_and_outcome(pipeline, config, S060ChildOutcome::Complete)
}

fn s060_hook_stack_with_config_and_outcome(
    pipeline: simulacra_hooks::HookPipeline,
    config: SimulacraConfig,
    outcome: S060ChildOutcome,
) -> S060HookStack {
    let mut capability = s060_capability(&["workspace"]);
    capability.shell = true;
    let journal = Arc::new(InMemoryJournalStorage::new());
    let observations = Arc::new(Mutex::new(Vec::new()));
    let parent_id = AgentId("parent-s060-hooks".into());
    let runtime: Arc<dyn simulacra_runtime::AcpChildRuntime> = Arc::new(S060RecordingAcpRuntime {
        observations: Arc::clone(&observations),
        journal: Arc::clone(&journal) as Arc<dyn JournalStorage>,
        parent_id: parent_id.clone(),
        outcome,
    });
    let factory = Arc::new(AgentTaskFactory {
        config,
        provider_kind: ProviderKind::Anthropic,
        vfs: Arc::new(MemoryFs::new()),
        journal: Arc::clone(&journal) as Arc<dyn JournalStorage>,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: capability.clone(),
        allowed_mcp_servers: None,
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: Some(Arc::new(pipeline)),
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: None,
        acp_child_runtime: Some(runtime),
    });
    let budget = Arc::new(Mutex::new(ResourceBudget::new(100, 10, Decimal::ZERO, 4)));
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&budget),
        factory,
    );
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    supervisor.set_root_agent_id(parent_id.clone());
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });
    let tool = SpawnAgentTool {
        sender: sender.clone(),
        allowed_placements: vec!["workspace".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id,
        parent_budget: Arc::clone(&budget),
        guidance: None,
    };
    S060HookStack {
        tool,
        join: JoinChildAgentTool {
            sender: sender.clone(),
            caller_id: AgentId("parent-s060-hooks".into()),
        },
        capability,
        journal,
        budget,
        observations,
        actor,
        sender,
    }
}

fn s060_hook_arguments() -> serde_json::Value {
    serde_json::json!({
        "placement": "workspace",
        "instructions": "  preserve hook instructions \n",
        "task": "  preserve hook task \n",
        "budget": s060_budget(10, 2, "0", 1)
    })
}

fn s060_expected_requested_budget() -> serde_json::Value {
    serde_json::json!({
        "max_tokens": 10,
        "max_turns": 2,
        "max_cost": "0",
        "max_sub_agents": 1
    })
}

fn s060_assert_no_accepted_spawn_effects(stack: &S060HookStack) {
    assert!(
        stack
            .observations
            .lock()
            .expect("runtime observations")
            .is_empty(),
        "rejected hook output must not construct the child runtime"
    );
    assert_eq!(stack.budget.lock().expect("budget lock").used_sub_agents, 0);
    let entries = stack
        .journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal");
    assert!(
        entries.iter().all(|entry| !matches!(
            entry.entry,
            JournalEntryKind::SubAgentSpawned { .. } | JournalEntryKind::SubAgentCompleted { .. }
        )),
        "rejected hook output must not journal accepted-child lifecycle entries"
    );
}

async fn s060_finish_stack(stack: S060HookStack) {
    drop(stack.tool);
    drop(stack.join);
    drop(stack.sender);
    stack
        .actor
        .await
        .expect("supervisor actor should stop cleanly");
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn s060_a15_a35_a36_before_hook_sees_exact_values_and_spawn_is_durable_before_runtime() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "capture".into(),
            calls: Arc::clone(&calls),
            behavior: Arc::new(|_, _| Ok(simulacra_hooks::Verdict::continue_unchanged())),
        }),
    );
    let stack = s060_hook_stack(pipeline);
    let acknowledgement = stack
        .tool
        .call(s060_hook_arguments(), &stack.capability)
        .await
        .expect("continued spawn should be accepted");
    let child_id = acknowledgement["child_id"].as_str().expect("child id");
    stack
        .join
        .call(serde_json::json!({"child_id": child_id}), &stack.capability)
        .await
        .expect("child should join");

    let calls = calls.lock().expect("hook call lock");
    let before = calls
        .iter()
        .find(|(_, phase, _)| *phase == simulacra_hooks::Phase::Before)
        .expect("before hook call");
    assert_eq!(
        before
            .2
            .as_object()
            .expect("before context object")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "backend",
            "budget",
            "capabilities",
            "instructions",
            "placement",
            "task"
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(before.2["placement"], "workspace");
    assert_eq!(before.2["backend"], "acp");
    assert_eq!(before.2["instructions"], "  preserve hook instructions \n");
    assert_eq!(before.2["task"], "  preserve hook task \n");
    assert_eq!(before.2["budget"], s060_expected_requested_budget());
    assert_eq!(
        before.2["capabilities"],
        serde_json::json!({
            "network": [],
            "mcp_tools": [],
            "shell": true,
            "javascript": false,
            "python": false,
            "paths_write": [],
            "paths_read": [],
            "spawn_placements": ["workspace"],
            "skill_patterns": [],
            "memory": {
                "enabled": false,
                "search_scopes": [],
                "write_scopes": []
            }
        }),
        "before-hook capability context must be the complete effective token"
    );
    drop(calls);

    let observations = stack.observations.lock().expect("runtime observations");
    assert_eq!(observations.len(), 1);
    assert!(observations[0].spawned_was_durable);
    assert_eq!(observations[0].request.task, "  preserve hook task \n");
    assert_eq!(observations[0].request.budget.max_tokens, 10);
    assert_eq!(observations[0].request.budget.max_turns, 2);
    assert_eq!(observations[0].request.budget.max_sub_agents, 1);
    assert!(observations[0].request.capability.shell);
    drop(observations);

    let entries = stack
        .journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal");
    let spawned = entries
        .iter()
        .filter_map(|entry| serde_json::to_value(&entry.entry).ok())
        .find(|entry| entry["type"] == "SubAgentSpawned")
        .expect("SubAgentSpawned entry");
    assert_eq!(
        spawned
            .as_object()
            .expect("spawn entry object")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "backend",
            "child_id",
            "instructions",
            "placement",
            "task",
            "type"
        ]
        .into_iter()
        .collect(),
        "v3 spawned payload must contain exactly the S060 fields"
    );
    assert_eq!(spawned["placement"], "workspace");
    assert_eq!(spawned["child_id"], acknowledgement["child_id"]);
    assert_eq!(spawned["backend"], "acp");
    assert_eq!(spawned["task"], "  preserve hook task \n");
    assert_eq!(spawned["instructions"], "  preserve hook instructions \n");
    assert!(spawned.get("agent_type").is_none());
    assert!(spawned.get("system_prompt").is_none());
    assert!(entries.iter().all(|entry| entry.schema_version == 3));
    s060_finish_stack(stack).await;
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn s060_a15_a36_omitted_instructions_are_null_and_capabilities_are_still_complete() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "capture-omitted".into(),
            calls: Arc::clone(&calls),
            behavior: Arc::new(|_, _| Ok(simulacra_hooks::Verdict::continue_unchanged())),
        }),
    );
    let stack = s060_hook_stack(pipeline);
    let mut arguments = s060_hook_arguments();
    arguments
        .as_object_mut()
        .expect("arguments object")
        .remove("instructions");
    let acknowledgement = stack
        .tool
        .call(arguments, &stack.capability)
        .await
        .expect("spawn without instructions should be accepted");
    stack
        .join
        .call(
            serde_json::json!({"child_id": acknowledgement["child_id"]}),
            &stack.capability,
        )
        .await
        .expect("child should join");

    let calls = calls.lock().expect("hook calls");
    let before = calls
        .iter()
        .find(|(_, phase, _)| *phase == simulacra_hooks::Phase::Before)
        .expect("before hook");
    assert!(before.2["instructions"].is_null());
    assert!(before.2["capabilities"].is_object());
    drop(calls);

    let entries = stack
        .journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal");
    let spawned = entries
        .iter()
        .map(|entry| serde_json::to_value(&entry.entry).expect("journal entry JSON"))
        .find(|entry| entry["type"] == "SubAgentSpawned")
        .expect("spawned entry");
    assert!(spawned["instructions"].is_null());
    s060_finish_stack(stack).await;
}

#[tokio::test]
async fn s060_a36_a40_before_hook_deny_has_no_accepted_spawn_effects() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "deny-policy".into(),
            calls: Arc::clone(&calls),
            behavior: Arc::new(|phase, _| match phase {
                simulacra_hooks::Phase::Before => {
                    Ok(simulacra_hooks::Verdict::Deny("blocked".into()))
                }
                simulacra_hooks::Phase::After => Ok(simulacra_hooks::Verdict::continue_unchanged()),
            }),
        }),
    );
    let stack = s060_hook_stack(pipeline);
    let error = stack
        .tool
        .call(s060_hook_arguments(), &stack.capability)
        .await
        .expect_err("before-hook denial should fail spawn")
        .to_string();
    assert!(error.contains("blocked"));
    s060_assert_no_accepted_spawn_effects(&stack);
    let entries = stack
        .journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal");
    let encoded = entries
        .iter()
        .map(|entry| serde_json::to_value(&entry.entry).expect("journal entry JSON"))
        .collect::<Vec<_>>();
    assert!(encoded.iter().any(|entry| entry["type"] == "HookDenial"));
    assert!(entries.iter().all(|entry| entry.schema_version == 3));
    assert!(
        !encoded
            .iter()
            .any(|entry| entry["type"] == "SubAgentSpawned")
    );
    s060_finish_stack(stack).await;
}

async fn s060_assert_named_before_hook_denial(
    name: &str,
    behavior: S060HookBehavior,
    expected_reason: &str,
) {
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: name.into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior,
        }),
    );
    let stack = s060_hook_stack(pipeline);
    stack
        .tool
        .call(s060_hook_arguments(), &stack.capability)
        .await
        .expect_err("named before-hook rejection should fail spawn");
    s060_assert_no_accepted_spawn_effects(&stack);

    let entries = stack
        .journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal");
    let denial = entries
        .iter()
        .find_map(|entry| match &entry.entry {
            JournalEntryKind::HookDenial {
                hook_name,
                operation,
                reason,
            } => Some((hook_name, operation, reason)),
            _ => None,
        })
        .expect("named rejection should journal HookDenial");
    assert_eq!(
        denial.0, name,
        "journal must retain the configured hook name"
    );
    assert_eq!(denial.1, "spawn");
    assert_eq!(denial.2, expected_reason);
    s060_finish_stack(stack).await;
}

#[tokio::test]
async fn s060_named_before_hook_deny_journals_the_actual_hook_and_exact_reason() {
    s060_assert_named_before_hook_denial(
        "named-deny-policy",
        Arc::new(|phase, _| match phase {
            simulacra_hooks::Phase::Before => Ok(simulacra_hooks::Verdict::Deny(
                "blocked by named policy".into(),
            )),
            simulacra_hooks::Phase::After => Ok(simulacra_hooks::Verdict::continue_unchanged()),
        }),
        "blocked by named policy",
    )
    .await;
}

#[tokio::test]
async fn s060_named_before_hook_timeout_journals_the_actual_hook_and_exact_reason() {
    s060_assert_named_before_hook_denial(
        "named-timeout-policy",
        Arc::new(|phase, _| match phase {
            simulacra_hooks::Phase::Before => Err(simulacra_hooks::HookError::Timeout {
                hook: "named-timeout-policy".into(),
                timeout_ms: 37,
            }),
            simulacra_hooks::Phase::After => Ok(simulacra_hooks::Verdict::continue_unchanged()),
        }),
        "hook timeout after 37ms (fail closed)",
    )
    .await;
}

#[tokio::test]
async fn s060_a40_before_hook_kill_is_journaled_without_accepting_a_child() {
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "kill-policy".into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior: Arc::new(|phase, _| match phase {
                simulacra_hooks::Phase::Before => {
                    Ok(simulacra_hooks::Verdict::Kill("terminate parent".into()))
                }
                simulacra_hooks::Phase::After => Ok(simulacra_hooks::Verdict::continue_unchanged()),
            }),
        }),
    );
    let stack = s060_hook_stack(pipeline);
    let error = stack
        .tool
        .call(s060_hook_arguments(), &stack.capability)
        .await
        .expect_err("before-hook kill should fail spawn")
        .to_string();
    assert!(error.contains("terminate parent"));
    s060_assert_no_accepted_spawn_effects(&stack);
    let raw_entries = stack
        .journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal");
    assert!(raw_entries.iter().all(|entry| entry.schema_version == 3));
    let entries = raw_entries
        .into_iter()
        .map(|entry| serde_json::to_value(entry.entry).expect("journal entry JSON"))
        .collect::<Vec<_>>();
    assert!(entries.iter().any(|entry| entry["type"] == "HookKill"));
    assert!(
        !entries
            .iter()
            .any(|entry| entry["type"] == "SubAgentSpawned")
    );
    s060_finish_stack(stack).await;
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn s060_a37_a38_hook_can_narrow_budget_and_capability_but_cannot_rewrite_workflow() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "attenuate".into(),
            calls: Arc::clone(&calls),
            behavior: Arc::new(|phase, context| {
                if phase == simulacra_hooks::Phase::After {
                    return Ok(simulacra_hooks::Verdict::continue_unchanged());
                }
                let mut narrowed = context.clone();
                narrowed["budget"]["max_tokens"] = serde_json::json!(5);
                if let Some(capabilities) = narrowed
                    .get_mut("capabilities")
                    .filter(|capabilities| capabilities.is_object())
                {
                    capabilities["shell"] = serde_json::json!(false);
                }
                Ok(simulacra_hooks::Verdict::Continue(Some(
                    narrowed.to_string(),
                )))
            }),
        }),
    );
    let stack = s060_hook_stack(pipeline);
    let acknowledgement = stack
        .tool
        .call(s060_hook_arguments(), &stack.capability)
        .await
        .expect("valid attenuation should be accepted");
    let before_capabilities_were_complete = calls
        .lock()
        .expect("hook call lock")
        .iter()
        .find(|(_, phase, _)| *phase == simulacra_hooks::Phase::Before)
        .is_some_and(|(_, _, context)| context["capabilities"].is_object());
    assert!(
        before_capabilities_were_complete,
        "attenuation requires the full effective capability object in the before context"
    );
    let child_id = acknowledgement["child_id"].as_str().expect("child id");
    stack
        .join
        .call(serde_json::json!({"child_id": child_id}), &stack.capability)
        .await
        .expect("attenuated child should join");
    let observations = stack.observations.lock().expect("runtime observations");
    assert_eq!(observations[0].request.budget.max_tokens, 5);
    assert!(!observations[0].request.capability.shell);
    drop(observations);
    s060_finish_stack(stack).await;

    for rewritten_field in ["placement", "backend", "instructions", "task", "invented"] {
        let mut pipeline = simulacra_hooks::HookPipeline::new();
        pipeline.add(
            simulacra_hooks::Operation::Spawn,
            Arc::new(S060RecordingHook {
                name: format!("rewrite-{rewritten_field}"),
                calls: Arc::new(Mutex::new(Vec::new())),
                behavior: Arc::new(move |phase, context| {
                    if phase == simulacra_hooks::Phase::After {
                        return Ok(simulacra_hooks::Verdict::continue_unchanged());
                    }
                    let mut rewritten = context.clone();
                    rewritten[rewritten_field] = serde_json::json!("rewritten");
                    Ok(simulacra_hooks::Verdict::Continue(Some(
                        rewritten.to_string(),
                    )))
                }),
            }),
        );
        let stack = s060_hook_stack(pipeline);
        let error = stack
            .tool
            .call(s060_hook_arguments(), &stack.capability)
            .await
            .expect_err("workflow or unknown field rewrite must fail closed")
            .to_string();
        assert!(
            error.contains(rewritten_field),
            "error should name {rewritten_field}: {error}"
        );
        s060_assert_no_accepted_spawn_effects(&stack);
        s060_finish_stack(stack).await;
    }

    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "attempt-capability-grant".into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior: Arc::new(|phase, context| {
                if phase == simulacra_hooks::Phase::After {
                    return Ok(simulacra_hooks::Verdict::continue_unchanged());
                }
                let mut widened = context.clone();
                widened["capabilities"]["network"] =
                    serde_json::json!(["https://not-granted.example/**"]);
                Ok(simulacra_hooks::Verdict::Continue(Some(
                    widened.to_string(),
                )))
            }),
        }),
    );
    let stack = s060_hook_stack(pipeline);
    let error = stack
        .tool
        .call(s060_hook_arguments(), &stack.capability)
        .await
        .expect_err("hook must not grant a caller-omitted capability")
        .to_string();
    assert!(
        error.contains("capabil"),
        "widening error should be actionable: {error}"
    );
    s060_assert_no_accepted_spawn_effects(&stack);
    s060_finish_stack(stack).await;

    for (name, mutate) in [
        (
            "budget-widen",
            Arc::new(|context: &mut serde_json::Value| {
                context["budget"]["max_tokens"] = serde_json::json!(11);
            }) as Arc<dyn Fn(&mut serde_json::Value) + Send + Sync>,
        ),
        (
            "budget-zero-under-finite-parent",
            Arc::new(|context: &mut serde_json::Value| {
                context["budget"]["max_tokens"] = serde_json::json!(0);
            }),
        ),
        (
            "descendant-placement-grant",
            Arc::new(|context: &mut serde_json::Value| {
                context["capabilities"]["spawn_placements"] =
                    serde_json::json!(["workspace", "ungranted"]);
            }),
        ),
    ] {
        let mut pipeline = simulacra_hooks::HookPipeline::new();
        pipeline.add(
            simulacra_hooks::Operation::Spawn,
            Arc::new(S060RecordingHook {
                name: name.into(),
                calls: Arc::new(Mutex::new(Vec::new())),
                behavior: Arc::new(move |phase, context| {
                    if phase == simulacra_hooks::Phase::After {
                        return Ok(simulacra_hooks::Verdict::continue_unchanged());
                    }
                    let mut invalid = context.clone();
                    mutate(&mut invalid);
                    Ok(simulacra_hooks::Verdict::Continue(Some(
                        invalid.to_string(),
                    )))
                }),
            }),
        );
        let stack = s060_hook_stack(pipeline);
        let error = stack
            .tool
            .call(s060_hook_arguments(), &stack.capability)
            .await
            .expect_err("hook widening or invalid zero must fail")
            .to_string();
        assert!(
            error.contains("budget") || error.contains("capabil") || error.contains("placement"),
            "hook revalidation error should be actionable: {error}"
        );
        s060_assert_no_accepted_spawn_effects(&stack);
        s060_finish_stack(stack).await;
    }
}

#[tokio::test]
async fn s060_a39_after_hooks_run_in_reverse_before_join_and_cannot_rewrite_terminal_result() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    for name in ["outer", "inner"] {
        pipeline.add(
            simulacra_hooks::Operation::Spawn,
            Arc::new(S060RecordingHook {
                name: name.into(),
                calls: Arc::clone(&calls),
                behavior: Arc::new(|phase, context| {
                    if phase == simulacra_hooks::Phase::After {
                        let mut modified = context.clone();
                        modified["result"] = serde_json::json!("failed");
                        return Ok(simulacra_hooks::Verdict::Continue(Some(
                            modified.to_string(),
                        )));
                    }
                    Ok(simulacra_hooks::Verdict::continue_unchanged())
                }),
            }),
        );
    }
    let stack = s060_hook_stack(pipeline);
    let acknowledgement = stack
        .tool
        .call(s060_hook_arguments(), &stack.capability)
        .await
        .expect("spawn should be accepted");
    let terminal = stack
        .join
        .call(
            serde_json::json!({"child_id": acknowledgement["child_id"]}),
            &stack.capability,
        )
        .await
        .expect("join should return original terminal result");
    assert_eq!(terminal["status"], "completed");
    let after_calls = calls
        .lock()
        .expect("hook call lock")
        .iter()
        .filter(|(_, phase, _)| *phase == simulacra_hooks::Phase::After)
        .map(|(name, _, context)| (name.clone(), context.clone()))
        .collect::<Vec<_>>();
    let after_order = after_calls
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(after_order, vec!["inner", "outer"]);
    for (_, context) in after_calls {
        assert_eq!(
            context
                .as_object()
                .expect("after context object")
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["backend", "child_id", "placement", "result", "tokens_used"]
                .into_iter()
                .collect()
        );
    }
    s060_finish_stack(stack).await;
}

async fn s060_assert_after_hook_terminal_mapping(
    outcome: S060ChildOutcome,
    expected_status: &'static str,
    expected_exit_reason: &'static str,
    expected_message: &'static str,
    expected_roster_status: serde_json::Value,
    expected_journal_success: bool,
) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: format!("rewrite-{expected_status}-result"),
            calls: Arc::clone(&calls),
            behavior: Arc::new(move |phase, context| {
                if phase == simulacra_hooks::Phase::Before {
                    return Ok(simulacra_hooks::Verdict::continue_unchanged());
                }
                let mut rewritten = context.clone();
                rewritten["result"] = serde_json::json!("completed");
                Ok(simulacra_hooks::Verdict::Continue(Some(
                    rewritten.to_string(),
                )))
            }),
        }),
    );
    let stack = s060_hook_stack_with_outcome(pipeline, outcome);
    let acknowledgement = stack
        .tool
        .call(s060_hook_arguments(), &stack.capability)
        .await
        .expect("spawn should be accepted before the child reaches its terminal state");
    let terminal = stack
        .join
        .call(
            serde_json::json!({"child_id": acknowledgement["child_id"]}),
            &stack.capability,
        )
        .await
        .expect("the typed child terminal result should remain joinable");
    let roster = ListChildAgentTool {
        sender: stack.sender.clone(),
        caller_id: AgentId("parent-s060-hooks".into()),
    }
    .call(serde_json::json!({}), &stack.capability)
    .await
    .expect("terminal child roster should remain queryable");
    let roster = roster.as_array().expect("child roster array");
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0]["child_id"], acknowledgement["child_id"]);
    assert_eq!(roster[0]["status"], expected_roster_status);
    let after_context = calls
        .lock()
        .expect("hook calls")
        .iter()
        .find(|(_, phase, _)| *phase == simulacra_hooks::Phase::After)
        .map(|(_, _, context)| context.clone())
        .expect("after-hook context");
    let entries = stack
        .journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal after child terminal");
    let completion_success = entries.iter().find_map(|entry| match &entry.entry {
        JournalEntryKind::SubAgentCompleted { success, .. } => Some(*success),
        _ => None,
    });
    s060_finish_stack(stack).await;

    assert_eq!(after_context["result"], expected_status);
    assert_eq!(after_context["tokens_used"], 5);
    assert_eq!(terminal["status"], expected_status);
    assert_eq!(terminal["exit_reason"], expected_exit_reason);
    assert_eq!(terminal["message"], expected_message);
    assert_eq!(terminal["token_usage"]["input_tokens"], 3);
    assert_eq!(terminal["token_usage"]["output_tokens"], 2);
    assert_eq!(completion_success, Some(expected_journal_success));
}

#[tokio::test]
async fn s060_a39_after_hook_maps_complete_exit_and_success_audit_exactly() {
    s060_assert_after_hook_terminal_mapping(
        S060ChildOutcome::Complete,
        "completed",
        "completed",
        "child summary",
        serde_json::json!({"completed": "child summary"}),
        true,
    )
    .await;
}

#[tokio::test]
async fn s060_a39_after_hook_maps_max_turns_to_completed_success_exactly() {
    s060_assert_after_hook_terminal_mapping(
        S060ChildOutcome::MaxTurns,
        "completed",
        "max_turns",
        "child max-turns result",
        serde_json::json!({"completed": "child max-turns result"}),
        true,
    )
    .await;
}

#[tokio::test]
async fn s060_a39_after_hook_maps_budget_exhausted_to_completed_success_exactly() {
    s060_assert_after_hook_terminal_mapping(
        S060ChildOutcome::BudgetExhausted,
        "completed",
        "budget_exhausted",
        "child budget-exhausted result",
        serde_json::json!({"completed": "child budget-exhausted result"}),
        true,
    )
    .await;
}

#[tokio::test]
async fn s060_a39_after_hook_maps_error_exit_to_failed_without_mutating_typed_result() {
    s060_assert_after_hook_terminal_mapping(
        S060ChildOutcome::ErrorExit,
        "failed",
        "error",
        "partial child failure result",
        serde_json::json!({"failed": "child provider failed"}),
        false,
    )
    .await;
}

#[tokio::test]
async fn s060_a39_after_hook_maps_guardrail_to_failed_non_success_exactly() {
    s060_assert_after_hook_terminal_mapping(
        S060ChildOutcome::GuardrailTripped,
        "failed",
        "guardrail_tripped:child guardrail",
        "child guardrail result",
        serde_json::json!({"failed": "child guardrail result"}),
        false,
    )
    .await;
}

#[tokio::test]
async fn s060_a39_after_hook_maps_cancelled_exit_without_mutating_typed_result() {
    s060_assert_after_hook_terminal_mapping(
        S060ChildOutcome::Cancelled,
        "cancelled",
        "cancelled",
        "child cancellation result",
        serde_json::json!({"cancelled": "child cancellation result"}),
        false,
    )
    .await;
}

#[tokio::test]
async fn s060_a39_child_policy_kill_maps_to_failed_everywhere_and_unsuccessful_audit() {
    s060_assert_after_hook_terminal_mapping(
        S060ChildOutcome::PolicyKill,
        "failed",
        "policy_kill",
        "child policy kill result",
        serde_json::json!({"failed": "child policy kill result"}),
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s060_a39_after_hook_blocks_terminal_cache_and_completed_journal_without_mutating_result() {
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = Arc::new(Mutex::new(release_rx));
    let after_context = Arc::new(Mutex::new(None));
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "blocking-after".into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior: {
                let release_rx = Arc::clone(&release_rx);
                let after_context = Arc::clone(&after_context);
                Arc::new(move |phase, context| {
                    if phase == simulacra_hooks::Phase::Before {
                        return Ok(simulacra_hooks::Verdict::continue_unchanged());
                    }
                    *after_context.lock().expect("after context lock") = Some(context.clone());
                    entered_tx.send(()).expect("test should await after hook");
                    release_rx
                        .lock()
                        .expect("release receiver lock")
                        .recv()
                        .expect("test should release after hook");
                    let mut attempted_rewrite = context.clone();
                    attempted_rewrite["result"] = serde_json::json!("failed");
                    attempted_rewrite["tokens_used"] = serde_json::json!(999);
                    Ok(simulacra_hooks::Verdict::Continue(Some(
                        attempted_rewrite.to_string(),
                    )))
                })
            },
        }),
    );
    let stack = s060_hook_stack(pipeline);
    let acknowledgement = stack
        .tool
        .call(s060_hook_arguments(), &stack.capability)
        .await
        .expect("spawn should acknowledge before terminal after-hook");
    let child_id = acknowledgement["child_id"]
        .as_str()
        .expect("child id")
        .to_string();

    tokio::task::spawn_blocking(move || {
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("child should reach blocking after-hook")
    })
    .await
    .expect("after-hook waiter should join");

    let status = ChildStatusTool {
        sender: stack.sender.clone(),
        caller_id: AgentId("parent-s060-hooks".into()),
    }
    .call(serde_json::json!({"child_id": child_id}), &stack.capability)
    .await
    .expect("accepted child status should remain available");
    assert_eq!(status["status"], "running");
    assert_eq!(status["ready"], false);
    assert_eq!(status["placement"], "workspace");
    let blocked_entries = stack
        .journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal while after-hook blocked");
    assert!(
        blocked_entries
            .iter()
            .any(|entry| matches!(entry.entry, JournalEntryKind::SubAgentSpawned { .. }))
    );
    assert!(
        blocked_entries
            .iter()
            .all(|entry| !matches!(entry.entry, JournalEntryKind::SubAgentCompleted { .. }))
    );

    release_tx.send(()).expect("release after hook");
    let terminal = stack
        .join
        .call(serde_json::json!({"child_id": child_id}), &stack.capability)
        .await
        .expect("original terminal result should become joinable");
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["exit_reason"], "completed");
    assert_eq!(terminal["message"], "child summary");
    assert_eq!(terminal["token_usage"]["input_tokens"], 3);
    assert_eq!(terminal["token_usage"]["output_tokens"], 2);

    let after = after_context
        .lock()
        .expect("after context lock")
        .clone()
        .expect("after hook context");
    assert_eq!(
        after
            .as_object()
            .expect("after context object")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        ["backend", "child_id", "placement", "result", "tokens_used"]
            .into_iter()
            .collect()
    );
    assert_eq!(after["child_id"], child_id);
    assert_eq!(after["placement"], "workspace");
    assert_eq!(after["backend"], "acp");
    assert_eq!(after["result"], "completed");
    assert_eq!(after["tokens_used"].as_u64(), Some(5));

    let completed_entries = stack
        .journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal after completion");
    assert!(
        completed_entries
            .iter()
            .any(|entry| matches!(entry.entry, JournalEntryKind::SubAgentCompleted { .. }))
    );
    assert!(
        completed_entries
            .iter()
            .all(|entry| entry.schema_version == 3)
    );
    s060_finish_stack(stack).await;
}

#[tokio::test]
async fn s060_a40_after_hook_kill_preserves_original_cached_and_journaled_terminal_result() {
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "after-kill".into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior: Arc::new(|phase, _| match phase {
                simulacra_hooks::Phase::Before => {
                    Ok(simulacra_hooks::Verdict::continue_unchanged())
                }
                simulacra_hooks::Phase::After => Ok(simulacra_hooks::Verdict::Kill(
                    "terminate parent after child".into(),
                )),
            }),
        }),
    );
    let stack = s060_hook_stack(pipeline);
    let acknowledgement = stack
        .tool
        .call(s060_hook_arguments(), &stack.capability)
        .await
        .expect("after-hook kill must not erase the accepted child handle");
    let terminal = stack
        .join
        .call(
            serde_json::json!({"child_id": acknowledgement["child_id"]}),
            &stack.capability,
        )
        .await
        .expect("original child result must remain joinable");
    assert_eq!(terminal["status"], "completed");

    let raw_entries = stack
        .journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal");
    assert!(raw_entries.iter().all(|entry| entry.schema_version == 3));
    let entries = raw_entries
        .into_iter()
        .map(|entry| serde_json::to_value(entry.entry).expect("journal entry JSON"))
        .collect::<Vec<_>>();
    assert!(entries.iter().any(|entry| entry["type"] == "HookKill"));
    assert!(
        entries
            .iter()
            .any(|entry| entry["type"] == "SubAgentCompleted")
    );
    s060_finish_stack(stack).await;
}

fn s060_parent_provider_responses() -> Vec<ProviderResponse> {
    vec![
        ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: "parent survived spawn denial".into(),
                tool_calls: vec![],
                tool_call_id: None,
                provider_content: vec![],
            },
            token_usage: TokenUsage {
                input_tokens: 7,
                output_tokens: 3,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
            },
            finish_reason: FinishReason::EndTurn,
            provider_response_id: Some("s060-parent-final".into()),
            model: "parent-model".into(),
        },
        ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCallMessage {
                    id: "s060-parent-spawn".into(),
                    name: "spawn_agent".into(),
                    arguments: s060_hook_arguments(),
                }],
                tool_call_id: None,
                provider_content: vec![],
            },
            token_usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 2,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
            },
            finish_reason: FinishReason::ToolUse,
            provider_response_id: Some("s060-parent-tool".into()),
            model: "parent-model".into(),
        },
    ]
}

struct S060BarrierParentProvider {
    calls: AtomicUsize,
    entered_second_call: Arc<tokio::sync::Barrier>,
    release_second_call: Arc<tokio::sync::Barrier>,
}

impl Provider for S060BarrierParentProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let entered_second_call = Arc::clone(&self.entered_second_call);
        let release_second_call = Arc::clone(&self.release_second_call);
        Box::pin(async move {
            match call {
                0 => Ok(ProviderResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: String::new(),
                        tool_calls: vec![ToolCallMessage {
                            id: "s060-parent-concurrent-spawn".into(),
                            name: "spawn_agent".into(),
                            arguments: s060_hook_arguments(),
                        }],
                        tool_call_id: None,
                        provider_content: vec![],
                    },
                    token_usage: TokenUsage {
                        input_tokens: 5,
                        output_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                    },
                    finish_reason: FinishReason::ToolUse,
                    provider_response_id: Some("s060-parent-concurrent-tool".into()),
                    model: "parent-model".into(),
                }),
                1 => {
                    entered_second_call.wait().await;
                    release_second_call.wait().await;
                    Ok(ProviderResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: "parent response after concurrent child completion".into(),
                            tool_calls: vec![],
                            tool_call_id: None,
                            provider_content: vec![],
                        },
                        token_usage: TokenUsage {
                            input_tokens: 7,
                            output_tokens: 3,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                        },
                        finish_reason: FinishReason::EndTurn,
                        provider_response_id: Some("s060-parent-concurrent-final".into()),
                        model: "parent-model".into(),
                    })
                }
                _ => panic!("barrier provider should receive exactly two calls"),
            }
        })
    }
}

fn s060_parent_loop(tool: SpawnAgentTool, journal: Arc<InMemoryJournalStorage>) -> AgentLoop {
    s060_parent_loop_with_provider(
        tool,
        journal,
        Box::new(FakeProvider::new(s060_parent_provider_responses())),
    )
}

fn s060_parent_loop_with_provider(
    tool: SpawnAgentTool,
    journal: Arc<InMemoryJournalStorage>,
    provider: Box<dyn Provider>,
) -> AgentLoop {
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(tool))
        .expect("spawn tool should register");
    AgentLoop::with_clock_and_replay(
        AgentLoopConfig {
            agent_id: AgentId("parent-s060-hooks".into()),
            system_prompt: "You are the spawning parent.".into(),
            model: "parent-model".into(),
            max_turns: 4,
            capability: s060_capability(&["workspace"]),
        },
        provider,
        tools,
        Box::new(PassthroughContext),
        Arc::clone(&journal) as Arc<dyn JournalStorage>,
        ResourceBudget::new(100, 4, Decimal::ZERO, 1),
        Box::new(simulacra_types::SystemClock),
        None,
    )
}

#[tokio::test]
async fn s060_a40_before_hook_deny_returns_tool_error_and_parent_continues_running() {
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "deny-parent".into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior: Arc::new(|phase, _| match phase {
                simulacra_hooks::Phase::Before => Ok(simulacra_hooks::Verdict::Deny(
                    "deny but continue parent".into(),
                )),
                simulacra_hooks::Phase::After => Ok(simulacra_hooks::Verdict::continue_unchanged()),
            }),
        }),
    );
    let stack = s060_hook_stack(pipeline);
    let S060HookStack {
        tool,
        join,
        capability: _,
        journal,
        budget,
        observations,
        actor,
        sender,
    } = stack;
    let mut parent = s060_parent_loop(tool, Arc::clone(&journal));
    let output = parent
        .run("delegate one bounded task")
        .await
        .expect("Deny is a tool error, not a parent-loop failure");
    assert_eq!(output.exit_reason, ExitReason::Complete);
    assert!(output.messages.iter().any(|message| {
        message.role == Role::Tool
            && message.content.contains("deny but continue parent")
            && message.content.starts_with("ERROR:")
    }));
    assert_eq!(
        output.messages.last().expect("final response").content,
        "parent survived spawn denial"
    );
    assert!(observations.lock().expect("observations").is_empty());
    assert_eq!(budget.lock().expect("budget").used_sub_agents, 0);
    let entries = journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal");
    assert!(entries.iter().all(|entry| entry.schema_version == 3));
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry.entry, JournalEntryKind::HookDenial { .. }))
    );
    assert!(
        entries
            .iter()
            .all(|entry| !matches!(entry.entry, JournalEntryKind::SubAgentSpawned { .. }))
    );
    drop(parent);
    drop(join);
    drop(sender);
    actor.await.expect("supervisor actor should stop");
}

#[tokio::test]
async fn s060_a40_before_hook_kill_terminates_real_parent_loop_with_policy_kill() {
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "kill-parent-before".into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior: Arc::new(|phase, _| match phase {
                simulacra_hooks::Phase::Before => Ok(simulacra_hooks::Verdict::Kill(
                    "stop spawning parent".into(),
                )),
                simulacra_hooks::Phase::After => Ok(simulacra_hooks::Verdict::continue_unchanged()),
            }),
        }),
    );
    let stack = s060_hook_stack(pipeline);
    let S060HookStack {
        tool,
        join,
        capability: _,
        journal,
        budget,
        observations,
        actor,
        sender,
    } = stack;
    let mut parent = s060_parent_loop(tool, Arc::clone(&journal));
    let output = parent
        .run("delegate one bounded task")
        .await
        .expect("policy kill should be a typed terminal output");
    assert_eq!(
        output.exit_reason,
        ExitReason::PolicyKill {
            hook: "kill-parent-before".into(),
            reason: "stop spawning parent".into()
        }
    );
    assert!(observations.lock().expect("observations").is_empty());
    assert_eq!(budget.lock().expect("budget").used_sub_agents, 0);
    let entries = journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal");
    assert!(entries.iter().all(|entry| entry.schema_version == 3));
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry.entry, JournalEntryKind::HookKill { .. }))
    );
    assert!(
        entries
            .iter()
            .all(|entry| !matches!(entry.entry, JournalEntryKind::SubAgentSpawned { .. }))
    );
    drop(parent);
    drop(join);
    drop(sender);
    actor.await.expect("supervisor actor should stop");
}

#[tokio::test]
async fn s060_a40_after_hook_kill_terminates_real_parent_and_keeps_child_terminal_result() {
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "kill-parent-after".into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior: Arc::new(|phase, _| match phase {
                simulacra_hooks::Phase::Before => {
                    Ok(simulacra_hooks::Verdict::continue_unchanged())
                }
                simulacra_hooks::Phase::After => Ok(simulacra_hooks::Verdict::Kill(
                    "stop parent after child".into(),
                )),
            }),
        }),
    );
    let stack = s060_hook_stack(pipeline);
    let S060HookStack {
        tool,
        join,
        capability,
        journal,
        budget: _,
        observations: _,
        actor,
        sender,
    } = stack;
    let mut parent = s060_parent_loop(tool, Arc::clone(&journal));
    let output = parent
        .run("delegate one bounded task")
        .await
        .expect("policy kill should be a typed terminal output");
    assert_eq!(
        output.exit_reason,
        ExitReason::PolicyKill {
            hook: "kill-parent-after".into(),
            reason: "stop parent after child".into()
        }
    );
    let entries = journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal");
    let child_id = entries
        .iter()
        .filter_map(|entry| serde_json::to_value(&entry.entry).ok())
        .find(|entry| entry["type"] == "SubAgentSpawned")
        .and_then(|entry| entry["child_id"].as_str().map(str::to_owned))
        .expect("accepted child id");
    let terminal = join
        .call(serde_json::json!({"child_id": child_id}), &capability)
        .await
        .expect("after-hook kill must preserve typed child result");
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["exit_reason"], "completed");
    assert_eq!(terminal["message"], "child summary");
    let entries = journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal after join");
    assert!(entries.iter().all(|entry| entry.schema_version == 3));
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry.entry, JournalEntryKind::HookKill { .. }))
    );
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry.entry, JournalEntryKind::SubAgentCompleted { .. }))
    );
    drop(parent);
    drop(join);
    drop(sender);
    actor.await.expect("supervisor actor should stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s060_a40_concurrent_after_hook_kill_interrupts_parent_mid_provider_and_preserves_child() {
    let child_release = Arc::new(tokio::sync::Barrier::new(2));
    let parent_entered = Arc::new(tokio::sync::Barrier::new(2));
    let parent_release = Arc::new(tokio::sync::Barrier::new(2));
    let (kill_seen_tx, kill_seen_rx) = std::sync::mpsc::sync_channel(1);
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::Spawn,
        Arc::new(S060RecordingHook {
            name: "kill-parent-concurrently".into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            behavior: Arc::new(move |phase, _| match phase {
                simulacra_hooks::Phase::Before => {
                    Ok(simulacra_hooks::Verdict::continue_unchanged())
                }
                simulacra_hooks::Phase::After => {
                    kill_seen_tx
                        .send(())
                        .expect("test should still await the after-hook kill");
                    Ok(simulacra_hooks::Verdict::Kill(
                        "stop parent during provider call".into(),
                    ))
                }
            }),
        }),
    );
    let stack = s060_hook_stack_with_outcome(
        pipeline,
        S060ChildOutcome::CompleteAfter(Arc::clone(&child_release)),
    );
    let S060HookStack {
        tool,
        join,
        capability,
        journal,
        budget: _,
        observations: _,
        actor,
        sender,
    } = stack;
    let provider = S060BarrierParentProvider {
        calls: AtomicUsize::new(0),
        entered_second_call: Arc::clone(&parent_entered),
        release_second_call: Arc::clone(&parent_release),
    };
    let mut parent = s060_parent_loop_with_provider(tool, Arc::clone(&journal), Box::new(provider));
    let parent_task = tokio::spawn(async move {
        parent
            .run("delegate one bounded concurrent task")
            .await
            .expect("policy kill should be a typed terminal output")
    });

    tokio::time::timeout(Duration::from_secs(2), parent_entered.wait())
        .await
        .expect("parent should enter its no-tool-call provider turn after spawning the child");
    child_release.wait().await;
    tokio::task::spawn_blocking(move || {
        kill_seen_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("child completion should reach the after-hook while the parent call is pending")
    })
    .await
    .expect("after-hook observer should join");
    assert!(
        !parent_task.is_finished(),
        "the parent must still be inside the controlled provider call when the kill arrives"
    );
    parent_release.wait().await;
    let output = tokio::time::timeout(Duration::from_secs(2), parent_task)
        .await
        .expect("parent should terminate promptly after its provider call is released")
        .expect("parent task should join");

    let entries = journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal");
    let child_id = entries
        .iter()
        .find_map(|entry| match &entry.entry {
            JournalEntryKind::SubAgentSpawned { child_id, .. } => Some(child_id.0.clone()),
            _ => None,
        })
        .expect("accepted child should be journaled");
    let terminal = join
        .call(serde_json::json!({"child_id": child_id}), &capability)
        .await
        .expect("the child's original terminal result should remain joinable");
    let entries = journal
        .read_all(&AgentId("parent-s060-hooks".into()))
        .expect("parent journal after join");
    drop(join);
    drop(sender);
    actor.await.expect("supervisor actor should stop");

    assert_eq!(
        output.exit_reason,
        ExitReason::PolicyKill {
            hook: "kill-parent-concurrently".into(),
            reason: "stop parent during provider call".into(),
        }
    );
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["exit_reason"], "completed");
    assert_eq!(terminal["message"], "child summary");
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry.entry, JournalEntryKind::HookKill { .. }))
    );
    assert!(entries.iter().any(|entry| matches!(
        entry.entry,
        JournalEntryKind::SubAgentCompleted { success: true, .. }
    )));
}
