use super::*;

pub(super) struct ChildSpawnToolSpec {
    pub(super) sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
    pub(super) can_spawn: Vec<String>,
    pub(super) tiers: TierMap,
    pub(super) parent_model: String,
}

pub(super) struct ChildEnvironmentSpec<'a> {
    pub(super) inherited_vfs: Arc<dyn VirtualFs>,
    pub(super) inherited_journal: Arc<dyn JournalStorage>,
    pub(super) spawn_config: &'a SpawnConfig,
    pub(super) child_config: &'a AgentLoopConfig,
    pub(super) agent_type_name: &'a str,
    pub(super) pipeline: Option<Arc<simulacra_hooks::pipeline::HookPipeline>>,
    pub(super) script_executor: Option<simulacra_sandbox::ScriptExecutor>,
    pub(super) cell_configurator: Option<ChildCellConfigurator>,
    pub(super) tool_registrar: Option<ChildToolRegistrar>,
    pub(super) spawn_tool: Option<ChildSpawnToolSpec>,
    pub(super) parent_sink: Arc<dyn ActivitySink>,
}

pub(super) struct ChildEnvironment {
    pub(super) proc: ChildProcRuntime,
    pub(super) registry: simulacra_tool::ToolRegistry,
    pub(super) sink: Arc<dyn ActivitySink>,
}

pub(super) fn build_child_environment(
    spec: ChildEnvironmentSpec<'_>,
) -> Result<ChildEnvironment, simulacra_types::ToolError> {
    let child_proc = child_proc_runtime(
        Arc::clone(&spec.inherited_vfs),
        Arc::clone(&spec.inherited_journal),
        ChildProcSpec {
            agent_id: spec.spawn_config.agent_id.clone(),
            agent_name: spec.agent_type_name.to_string(),
            model: spec.child_config.model.clone(),
            parent_id: spec.spawn_config.parent_id.clone(),
            capability: spec.child_config.capability.clone(),
            budget: spec.spawn_config.budget.clone(),
            pipeline: spec.pipeline.clone(),
        },
    );
    let cell = build_child_cell(&child_proc, spec.child_config, &spec);
    let registry = build_child_registry(&cell, Arc::clone(&child_proc.budget), &spec)?;
    child_proc.tools.set(registry.definitions());

    let sink: Arc<dyn ActivitySink> = Arc::new(ForwardingActivitySink::new(
        spec.spawn_config.agent_id.0.clone(),
        spec.agent_type_name.to_string(),
        spec.parent_sink,
    ));

    Ok(ChildEnvironment {
        proc: child_proc,
        registry,
        sink,
    })
}

fn build_child_cell(
    child_proc: &ChildProcRuntime,
    child_config: &AgentLoopConfig,
    spec: &ChildEnvironmentSpec<'_>,
) -> Arc<simulacra_sandbox::AgentCell> {
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let mut cell = simulacra_sandbox::AgentCell::new(
        Arc::clone(&child_proc.vfs),
        child_config.capability.clone(),
        Arc::clone(&child_proc.budget),
        Arc::clone(&child_proc.journal),
        http_client,
    );
    if let Some(executor) = &spec.script_executor {
        cell.set_script_executor(executor.clone());
    }
    if let Some(configure_cell) = &spec.cell_configurator {
        configure_cell(&mut cell);
    }
    Arc::new(cell)
}

fn build_child_registry(
    cell: &Arc<simulacra_sandbox::AgentCell>,
    child_budget: Arc<Mutex<ResourceBudget>>,
    spec: &ChildEnvironmentSpec<'_>,
) -> Result<simulacra_tool::ToolRegistry, simulacra_types::ToolError> {
    let mut registry = simulacra_tool::ToolRegistry::new();
    simulacra_tool::register_builtins(&mut registry, Arc::clone(cell))?;
    if let Some(register_extra_tools) = &spec.tool_registrar {
        register_extra_tools(&mut registry, Arc::clone(cell))?;
    }
    if let Some(spawn_tool) = &spec.spawn_tool {
        let sender = spawn_tool.sender.clone();
        registry.register(Box::new(SpawnAgentTool {
            sender: sender.clone(),
            can_spawn: spawn_tool.can_spawn.clone(),
            activity_sink: Arc::clone(&spec.parent_sink),
            parent_id: spec.spawn_config.agent_id.clone(),
            tiers: spawn_tool.tiers.clone(),
            parent_budget: child_budget,
            parent_model: spawn_tool.parent_model.clone(),
        }))?;
        registry.register(Box::new(JoinChildAgentTool {
            sender: sender.clone(),
        }))?;
        registry.register(Box::new(CancelChildAgentTool { sender }))?;
    }
    Ok(registry)
}
