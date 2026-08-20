use super::*;

struct CountingActivitySink {
    inner: Arc<dyn ActivitySink>,
    tool_finishes: Arc<AtomicU64>,
}

impl CountingActivitySink {
    fn new(inner: Arc<dyn ActivitySink>, tool_finishes: Arc<AtomicU64>) -> Self {
        Self {
            inner,
            tool_finishes,
        }
    }
}

impl ActivitySink for CountingActivitySink {
    fn emit(&self, event: simulacra_types::ActivityEvent) {
        if matches!(event, simulacra_types::ActivityEvent::ToolFinish { .. }) {
            self.tool_finishes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.inner.emit(event);
    }
}

pub struct AgentTaskFactory {
    pub config: SimulacraConfig,
    pub provider_kind: ProviderKind,
    pub vfs: Arc<dyn VirtualFs>,
    pub journal: Arc<dyn JournalStorage>,
    pub activity_sink: Arc<dyn ActivitySink>,
    pub parent_capability: CapabilityToken,
    pub allowed_mcp_servers: Option<Vec<String>>,
    pub supervisor_sender: Option<tokio::sync::mpsc::Sender<SupervisorMessage>>,
    pub pipeline: Option<Arc<simulacra_hooks::pipeline::HookPipeline>>,
    pub script_executor: Option<simulacra_sandbox::ScriptExecutor>,
    pub child_cell_configurator: Option<ChildCellConfigurator>,
    pub child_tool_registrar: Option<ChildToolRegistrar>,
    pub child_provider_factory: Option<ChildProviderFactory>,
    pub acp_child_runtime: Option<Arc<dyn AcpChildRuntime>>,
}

impl crate::TaskFactory for AgentTaskFactory {
    fn validate_spawn_config(&self, spawn_config: &SpawnConfig) -> Result<(), RuntimeError> {
        let placement = self
            .config
            .child_placements
            .get(&spawn_config.placement)
            .ok_or_else(|| unknown_placement_error(&self.config, &spawn_config.placement))?;
        if placement.backend == AgentBackend::Native {
            native_placement_model(&spawn_config.placement, placement)?;
        }
        validate_placement_budget(&spawn_config.budget, placement)?;
        if placement.backend == AgentBackend::Acp && self.acp_child_runtime.is_none() {
            return Err(RuntimeError::AcpChildRuntimeMissing {
                placement: spawn_config.placement.clone(),
                acp_profile: placement
                    .acp_profile
                    .clone()
                    .unwrap_or_else(|| "<missing>".to_string()),
            });
        }
        Ok(())
    }

    fn placement_backend(&self, spawn_config: &SpawnConfig) -> AgentBackend {
        self.config
            .child_placements
            .get(&spawn_config.placement)
            .map_or(AgentBackend::Native, |placement| placement.backend)
    }

    fn prepare_spawn_config(&self, spawn_config: &mut SpawnConfig) -> Result<(), RuntimeError> {
        self.prepare_spawn_config_for_caller(spawn_config, &self.parent_capability)
    }

    fn prepare_spawn_config_for_caller(
        &self,
        spawn_config: &mut SpawnConfig,
        caller_capability: &CapabilityToken,
    ) -> Result<(), RuntimeError> {
        let placement = self
            .config
            .child_placements
            .get(&spawn_config.placement)
            .ok_or_else(|| unknown_placement_error(&self.config, &spawn_config.placement))?;
        let effective_capability = effective_spawn_capability(
            placement,
            spawn_config.capability.as_ref(),
            caller_capability,
        );
        run_spawn_before_hook(
            self.pipeline.as_ref(),
            &self.journal,
            spawn_config,
            placement.backend,
            effective_capability,
        )?;
        validate_placement_budget(&spawn_config.budget, placement)
    }

    fn after_spawn(
        &self,
        spawn_config: &SpawnConfig,
        result: &crate::SpawnResult,
    ) -> Result<(), RuntimeError> {
        run_spawn_after_hook(
            self.pipeline.as_ref(),
            &self.journal,
            spawn_config,
            self.placement_backend(spawn_config),
            result,
        )
    }

    fn create_task(
        &self,
        spawn_config: SpawnConfig,
        cancellation: CancellationToken,
    ) -> BoxTaskFuture {
        let (input_queue, _input_handle) = AgentInputQueue::new();
        self.create_task_with_input(spawn_config, cancellation, input_queue)
    }

    fn create_task_with_input(
        &self,
        spawn_config: SpawnConfig,
        cancellation: CancellationToken,
        input_queue: AgentInputQueue,
    ) -> BoxTaskFuture {
        let budget = Arc::new(Mutex::new(spawn_config.budget.clone()));
        self.create_task_with_input_and_budget(spawn_config, cancellation, input_queue, budget)
    }

    fn create_task_with_input_and_budget(
        &self,
        spawn_config: SpawnConfig,
        cancellation: CancellationToken,
        input_queue: AgentInputQueue,
        child_budget: Arc<Mutex<ResourceBudget>>,
    ) -> BoxTaskFuture {
        let placement_config = self
            .config
            .child_placements
            .get(&spawn_config.placement)
            .cloned();
        let provider_kind = self.provider_kind.clone();
        let vfs = Arc::clone(&self.vfs);
        let journal = Arc::clone(&self.journal);
        let task = spawn_config.task.clone();
        let parent_sink = Arc::clone(&self.activity_sink);
        let parent_capability = self.parent_capability.clone();
        let supervisor_sender = self.supervisor_sender.clone();
        let pipeline = self.pipeline.clone();
        let script_executor = self.script_executor.clone();
        let child_cell_configurator = self.child_cell_configurator.clone();
        let child_tool_registrar = self.child_tool_registrar.clone();
        let child_provider_factory = self.child_provider_factory.clone();
        let acp_child_runtime = self.acp_child_runtime.clone();
        let runtime_config = self.config.clone();
        let allowed_mcp_servers = self.allowed_mcp_servers.clone();

        Box::pin(async move {
            let placement_config = placement_config
                .ok_or_else(|| unknown_placement_error(&runtime_config, &spawn_config.placement))?;
            let placement_name = spawn_config.placement.clone();
            // Supervisor preparation resolves placement ∩ immediate caller ∩
            // optional caller attenuation, then hooks may narrow it further.
            // Once present, that token is authoritative and must not be
            // recomputed (notably, omitted-memory inheritance is model-input
            // behavior and must not regrant memory removed by a hook).
            let effective_capability = spawn_config.capability.clone().unwrap_or_else(|| {
                effective_spawn_capability(&placement_config, None, &parent_capability)
            });

            if placement_config.backend == AgentBackend::Acp {
                let acp_profile = placement_config
                    .acp_profile
                    .clone()
                    .filter(|profile| !profile.trim().is_empty())
                    .ok_or_else(|| {
                        RuntimeError::Session(format!(
                            "placement {placement_name:?} has no acp_profile"
                        ))
                    })?;
                let runtime =
                    acp_child_runtime.ok_or_else(|| RuntimeError::AcpChildRuntimeMissing {
                        placement: placement_name.clone(),
                        acp_profile: acp_profile.clone(),
                    })?;
                let immediate_parent_sink = parent_sink
                    .immediate_parent_sink(&spawn_config.parent_id)
                    .unwrap_or_else(|| Arc::clone(&parent_sink));
                let sink: Arc<dyn ActivitySink> = Arc::new(ForwardingActivitySink::new_routed(
                    spawn_config.agent_id.0.clone(),
                    placement_name.clone(),
                    immediate_parent_sink,
                ));
                parent_sink.register_child_sink(spawn_config.agent_id.clone(), Arc::clone(&sink));
                let tool_finishes = Arc::new(AtomicU64::new(0));
                let sink: Arc<dyn ActivitySink> =
                    Arc::new(CountingActivitySink::new(sink, Arc::clone(&tool_finishes)));
                let request = AcpChildRequest {
                    child_id: spawn_config.agent_id.clone(),
                    parent_id: spawn_config.parent_id.clone(),
                    placement: placement_name.clone(),
                    acp_profile,
                    instructions: spawn_config.instructions.clone(),
                    task: task.clone(),
                    budget: spawn_config.budget.clone(),
                    capability: effective_capability,
                };
                let result = runtime
                    .start_child(request, cancellation, sink, input_queue)
                    .await
                    .map_err(RuntimeError::acp_child_runtime);
                let mut output = result?;
                let activity_tool_uses = tool_finishes.load(std::sync::atomic::Ordering::Relaxed);
                if activity_tool_uses > 0 {
                    let output_tool_uses = output.reported_tool_uses.unwrap_or_else(|| {
                        output
                            .messages
                            .iter()
                            .filter(|message| message.role == simulacra_types::Role::Tool)
                            .count() as u64
                    });
                    output.reported_tool_uses = Some(output_tool_uses.max(activity_tool_uses));
                }
                return Ok(output);
            }

            let model = native_placement_model(&placement_name, &placement_config)?;
            let system_prompt = spawn_config
                .instructions
                .clone()
                .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
            let mut descendant_placements = effective_capability
                .spawn_placements
                .iter()
                .filter(|placement| runtime_config.child_placements.contains_key(*placement))
                .cloned()
                .collect::<Vec<_>>();
            descendant_placements.sort();
            descendant_placements.dedup();
            let child_has_spawn_placements = !descendant_placements.is_empty();
            let child_config = AgentLoopConfig {
                agent_id: spawn_config.agent_id.clone(),
                system_prompt,
                model,
                max_turns: spawn_config.budget.max_turns,
                capability: effective_capability,
                context_token_limit: None,
            };
            let provider = build_child_provider(
                child_provider_factory.as_ref(),
                &provider_kind,
                &child_config.model,
            )?;
            let spawn_tool = if child_has_spawn_placements {
                supervisor_sender.map(|sender| ChildSpawnToolSpec {
                    sender,
                    allowed_placements: descendant_placements,
                })
            } else {
                None
            };
            let child_env = build_child_environment(ChildEnvironmentSpec {
                inherited_vfs: vfs,
                inherited_journal: journal,
                spawn_config: &spawn_config,
                child_config: &child_config,
                budget: child_budget,
                placement_name: &placement_name,
                pipeline: pipeline.clone(),
                script_executor,
                cell_configurator: child_cell_configurator,
                tool_registrar: child_tool_registrar,
                spawn_tool,
                parent_sink,
                runtime_config: Some(&runtime_config),
                skill_names: &placement_config.skills,
                allowed_mcp_servers: allowed_mcp_servers.as_deref(),
            })?;
            let mut child_loop = AgentLoop::new(
                child_config,
                provider,
                child_env.registry,
                Box::new(simulacra_context::ObservationMaskingStrategy::new(5)),
                child_env.proc.journal,
                spawn_config.budget,
                Some(child_env.sink),
                pipeline.clone(),
            );
            child_loop.set_proc_budget_mirror(child_env.proc.budget, child_env.proc.turn);
            child_loop.set_cancellation_token(cancellation);
            child_loop.set_input_queue(input_queue);
            child_loop.run(&task).await
        })
    }
}

fn effective_spawn_capability(
    placement: &simulacra_config::ChildPlacementConfig,
    requested: Option<&CapabilityToken>,
    parent: &CapabilityToken,
) -> CapabilityToken {
    let configured = build_child_placement_capability(placement);
    match requested {
        Some(requested) => configured.intersect(requested).intersect(parent),
        None => configured.intersect(parent),
    }
}

fn unknown_placement_error(config: &SimulacraConfig, requested: &str) -> RuntimeError {
    let mut available = config.child_placements.keys().cloned().collect::<Vec<_>>();
    available.sort();
    let available_suffix = if available.is_empty() {
        String::new()
    } else {
        format!("; available placements: {}", available.join(", "))
    };
    RuntimeError::Session(format!(
        "unknown child placement {requested:?}{available_suffix}"
    ))
}

fn native_placement_model(
    placement_name: &str,
    placement: &simulacra_config::ChildPlacementConfig,
) -> Result<String, RuntimeError> {
    placement
        .model
        .clone()
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            RuntimeError::Session(format!(
                "native child placement {placement_name:?} requires a non-blank model"
            ))
        })
}

fn validate_placement_budget(
    requested: &ResourceBudget,
    placement: &simulacra_config::ChildPlacementConfig,
) -> Result<(), RuntimeError> {
    validate_placement_limit("max_tokens", requested.max_tokens, placement.max_tokens)?;
    validate_placement_limit("max_turns", requested.max_turns, placement.max_turns)?;
    validate_placement_limit("max_cost", requested.max_cost, placement.max_cost)?;
    validate_placement_limit(
        "max_sub_agents",
        requested.max_sub_agents,
        placement.max_sub_agents,
    )
}

fn validate_placement_limit<T>(
    field: &str,
    requested: T,
    maximum: Option<T>,
) -> Result<(), RuntimeError>
where
    T: Copy + Default + PartialEq + PartialOrd + std::fmt::Display,
{
    let Some(maximum) = maximum.filter(|value| *value != T::default()) else {
        return Ok(());
    };
    if requested == T::default() {
        return Err(RuntimeError::Session(format!(
            "{field} requested {requested} (unlimited), but placement limit is {maximum}"
        )));
    }
    if requested > maximum {
        return Err(RuntimeError::Session(format!(
            "{field} requested {requested} exceeds placement limit {maximum}"
        )));
    }
    Ok(())
}

fn build_child_provider(
    child_provider_factory: Option<&ChildProviderFactory>,
    provider_kind: &ProviderKind,
    model: &str,
) -> Result<Box<dyn Provider>, RuntimeError> {
    match child_provider_factory {
        Some(factory) => factory(provider_kind, model),
        None => build_provider(provider_kind, model),
    }
}
