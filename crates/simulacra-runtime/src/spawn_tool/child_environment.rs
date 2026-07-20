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
    /// Full configured runtime surface used only for configured native child
    /// skill/MCP discovery. Generic leaf children deliberately receive None.
    pub(super) runtime_config: Option<&'a simulacra_config::SimulacraConfig>,
    pub(super) skill_names: &'a [String],
    pub(super) allowed_mcp_servers: Option<&'a [String]>,
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
    if let Some(config) = spec.runtime_config {
        register_child_skill_mcp_catalog(&mut registry, cell, spec, config)?;
    }
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
            guidance: None,
        }))?;
        registry.register(Box::new(JoinChildAgentTool {
            sender: sender.clone(),
        }))?;
        registry.register(Box::new(CancelChildAgentTool {
            sender: sender.clone(),
        }))?;
        registry.register(Box::new(SteerChildAgentTool {
            sender: sender.clone(),
        }))?;
        registry.register(Box::new(ChildStatusTool {
            sender: sender.clone(),
        }))?;
        registry.register(Box::new(ListChildAgentTool {
            sender: sender.clone(),
        }))?;
        registry.register(Box::new(WaitChildAgentTool {
            sender: sender.clone(),
        }))?;
        registry.register(Box::new(CloseChildAgentTool { sender }))?;
    }
    Ok(registry)
}

fn register_child_skill_mcp_catalog(
    registry: &mut simulacra_tool::ToolRegistry,
    cell: &Arc<simulacra_sandbox::AgentCell>,
    spec: &ChildEnvironmentSpec<'_>,
    config: &simulacra_config::SimulacraConfig,
) -> Result<(), simulacra_types::ToolError> {
    let skills = simulacra_tool::discover_and_filter_skills(
        &spec.inherited_vfs,
        spec.skill_names,
        &spec.child_config.capability,
        spec.agent_type_name,
    )
    .map_err(|error| simulacra_types::ToolError::ExecutionFailed(error.to_string()))?;

    let configured_names: std::collections::HashSet<&str> = config
        .mcp
        .as_ref()
        .into_iter()
        .flat_map(|mcp| &mcp.servers)
        .map(|server| server.name.as_str())
        .collect();
    let derived_allowlist;
    let allowed_servers = if let Some(allowed) = spec.allowed_mcp_servers {
        allowed
    } else if config.tenants.is_empty() {
        derived_allowlist = configured_names
            .iter()
            .map(|server| (*server).to_string())
            .collect::<Vec<_>>();
        &derived_allowlist
    } else {
        derived_allowlist = config
            .tenants
            .values()
            .find(|tenant| tenant.agent_type == spec.agent_type_name)
            .and_then(|tenant| tenant.mcp_servers.clone())
            .unwrap_or_default();
        &derived_allowlist
    };
    for skill in &skills {
        for server in &skill.mcp_servers {
            if !configured_names.contains(server.as_str()) {
                return Err(simulacra_types::ToolError::ExecutionFailed(format!(
                    "skill {:?} references unknown configured MCP server {:?}",
                    skill.name, server
                )));
            }
            if !allowed_servers.contains(server) {
                return Err(simulacra_types::ToolError::ExecutionFailed(format!(
                    "skill {:?} MCP server {:?} is denied by the effective server allow-list",
                    skill.name, server
                )));
            }
            if !mcp_capability_may_cover_server(&spec.child_config.capability.mcp_tools, server) {
                return Err(simulacra_types::ToolError::ExecutionFailed(format!(
                    "skill {:?} MCP server {:?} is denied by the child's attenuated capability",
                    skill.name, server
                )));
            }
        }
    }

    let Some(mcp) = config.mcp.as_ref().filter(|mcp| !mcp.servers.is_empty()) else {
        return if skills.iter().any(|skill| !skill.mcp_servers.is_empty()) {
            Err(simulacra_types::ToolError::ExecutionFailed(
                "configured child skill requires MCP but no MCP catalog is configured".into(),
            ))
        } else {
            register_child_skill_tool(registry, cell, skills, None)
        };
    };

    let descriptors = mcp
        .servers
        .iter()
        .filter(|server| allowed_servers.contains(&server.name))
        .filter(|server| {
            mcp_capability_may_cover_server(&spec.child_config.capability.mcp_tools, &server.name)
        })
        .filter_map(|server| {
            if server.transport.as_deref() == Some("wasm") {
                server.module.as_ref().map(|module| {
                    simulacra_mcp::McpServerDescriptor::wasm(
                        server.name.clone(),
                        simulacra_mcp::DeferredWasmMcpServerDescriptor {
                            module_path: std::path::PathBuf::from(module),
                            network_allowlist: server.network.clone(),
                            hooks: spec.pipeline.clone(),
                            journal: Some(Arc::clone(&spec.inherited_journal)),
                            agent_id: spec.spawn_config.agent_id.clone(),
                        },
                    )
                })
            } else {
                server.url.as_ref().map(|url| {
                    simulacra_mcp::McpServerDescriptor::network(
                        server.name.clone(),
                        url.clone(),
                        server.transport.clone(),
                    )
                })
            }
        })
        .collect();
    let catalog = simulacra_mcp::McpCatalog::with_journal(
        descriptors,
        Arc::clone(&spec.inherited_journal),
        spec.spawn_config.agent_id.clone(),
    )?;
    register_child_skill_tool(registry, cell, skills, Some(Arc::clone(&catalog)))?;
    registry.register(Box::new(simulacra_mcp::McpSearchTool::new(Arc::clone(
        &catalog,
    ))))?;
    registry.register(Box::new(simulacra_mcp::McpCallTool::new(catalog)))?;
    Ok(())
}

fn register_child_skill_tool(
    registry: &mut simulacra_tool::ToolRegistry,
    cell: &Arc<simulacra_sandbox::AgentCell>,
    skills: Vec<simulacra_tool::SkillMeta>,
    catalog: Option<Arc<simulacra_mcp::McpCatalog>>,
) -> Result<(), simulacra_types::ToolError> {
    if skills
        .iter()
        .any(|skill| !skill.disable_model_invocation && skill.allow_implicit_invocation)
    {
        let tool = simulacra_tool::SkillTool::new(Arc::clone(cell), skills);
        let tool = match catalog {
            Some(catalog) => tool.with_dependency_activator(
                catalog as Arc<dyn simulacra_types::SkillDependencyActivator>,
            ),
            None => tool,
        };
        registry.register(Box::new(tool))?;
    }
    Ok(())
}

fn mcp_capability_may_cover_server(patterns: &[String], server: &str) -> bool {
    patterns.iter().any(|pattern| {
        pattern
            .strip_prefix("mcp:")
            .and_then(|rest| rest.split_once(':'))
            .is_some_and(|(server_pattern, tool_pattern)| {
                !tool_pattern.is_empty() && (server_pattern == "*" || server_pattern == server)
            })
    })
}
