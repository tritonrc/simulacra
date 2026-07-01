use super::*;

// ---------------------------------------------------------------------------
// AgentTaskFactory
// ---------------------------------------------------------------------------

pub type ChildCellConfigurator = Arc<dyn Fn(&mut simulacra_sandbox::AgentCell) + Send + Sync>;

pub type ChildToolRegistrar =
    Arc<dyn Fn(&mut simulacra_tool::ToolRegistry, Arc<simulacra_sandbox::AgentCell>) + Send + Sync>;

/// Factory that creates child AgentLoop instances for the supervisor.
pub struct AgentTaskFactory {
    pub config: SimulacraConfig,
    pub provider_kind: ProviderKind,
    pub vfs: Arc<dyn VirtualFs>,
    pub journal: Arc<dyn JournalStorage>,
    /// S019: Parent's activity sink for creating ForwardingActivitySink
    /// on child agent spawns.
    pub activity_sink: Arc<dyn ActivitySink>,
    /// Parent's capability token for three-way capability intersection.
    /// The effective child capability = config_cap ∩ spawn_override ∩ parent_cap.
    #[allow(dead_code)]
    pub parent_capability: CapabilityToken,
    /// Supervisor channel sender — passed to child `SpawnAgentTool` instances
    /// so children with `spawn_types` can spawn their own descendants (S018 §173).
    pub supervisor_sender: Option<tokio::sync::mpsc::Sender<SupervisorMessage>>,
    /// The parent agent's model, used as fallback for generic sub-agents
    /// when no tier is specified or the tier is not found in config.
    pub parent_model: String,
    /// Governance hook pipeline, shared with child agents (S026).
    pub pipeline: Option<Arc<simulacra_hooks::pipeline::HookPipeline>>,
    /// Script executor for bounded concurrency control, shared across all agents.
    /// When present, child `AgentCell`s receive this executor so JS/Python/WASM
    /// scripts share the same concurrency semaphore as the root agent.
    pub script_executor: Option<simulacra_sandbox::ScriptExecutor>,
    /// Optional caller-provided hook for inheriting host mediation context
    /// that lives above the runtime crate, such as integration-backed fetch().
    pub child_cell_configurator: Option<ChildCellConfigurator>,
    /// Optional caller-provided hook for registering extra mediated tools that
    /// are feature- or crate-local to the embedding binary, such as `py_exec`.
    pub child_tool_registrar: Option<ChildToolRegistrar>,
}

impl crate::TaskFactory for AgentTaskFactory {
    fn create_task(
        &self,
        spawn_config: SpawnConfig,
        _cancellation: CancellationToken,
    ) -> BoxTaskFuture {
        let agent_type_config = spawn_config
            .agent_type
            .as_ref()
            .and_then(|at| self.config.agent_types.get(at))
            .cloned();

        let provider_kind = self.provider_kind.clone();
        let vfs = Arc::clone(&self.vfs);
        let journal = Arc::clone(&self.journal);
        let task = spawn_config.task.clone();
        let parent_sink = Arc::clone(&self.activity_sink);
        let parent_capability = self.parent_capability.clone();
        let supervisor_sender = self.supervisor_sender.clone();
        let tiers_config = self.config.tiers.clone();
        let parent_model = self.parent_model.clone();
        let pipeline = self.pipeline.clone();
        let script_executor = self.script_executor.clone();
        let child_cell_configurator = self.child_cell_configurator.clone();
        let child_tool_registrar = self.child_tool_registrar.clone();

        Box::pin(async move {
            // === GENERIC MODE ===
            if spawn_config.agent_type.is_none() {
                let system_prompt = spawn_config
                    .system_prompt
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());

                tracing::info!(
                    simulacra.agent.system_prompt_length = system_prompt.len(),
                    "generic agent spawned with inline system prompt"
                );

                let model =
                    resolve_tier_model(spawn_config.tier.as_deref(), &tiers_config, &parent_model);

                // Two-way capability intersection: parent ∩ override (no config layer).
                // W1: when the override doesn't author memory, inherit parent memory
                // before intersecting so the child doesn't silently lose memory access.
                let mut effective_capability = match spawn_config.capability {
                    Some(ref override_cap) => {
                        let override_with_memory =
                            inherit_memory_when_override_unset(override_cap, &parent_capability);
                        parent_capability.intersect(&override_with_memory)
                    }
                    None => parent_capability.clone(),
                };
                // Generic agents are leaf workers — explicitly zero out spawn_types
                // so the capability token reflects the invariant (not just the tool registry).
                effective_capability.spawn_types = vec![];

                let child_config = AgentLoopConfig {
                    agent_id: spawn_config.agent_id.clone(),
                    system_prompt,
                    model: model.clone(),
                    max_turns: spawn_config.budget.max_turns,
                    capability: effective_capability,
                };

                let provider: Box<dyn Provider> = match provider_kind {
                    ProviderKind::Anthropic => {
                        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
                            RuntimeError::Session("ANTHROPIC_API_KEY not set".into())
                        })?;
                        Box::new(AnthropicProvider::new(api_key, &model))
                    }
                    ProviderKind::OpenAI => {
                        let api_key = std::env::var("OPENAI_API_KEY")
                            .map_err(|_| RuntimeError::Session("OPENAI_API_KEY not set".into()))?;
                        Box::new(OpenAiProvider::new(api_key, &model))
                    }
                    ProviderKind::Ollama => Box::new(OpenAiProvider::new("ollama", &model)),
                };

                let child_proc = child_proc_runtime(
                    Arc::clone(&vfs),
                    Arc::clone(&journal),
                    ChildProcSpec {
                        agent_id: spawn_config.agent_id.clone(),
                        agent_name: "generic".to_string(),
                        model: model.clone(),
                        parent_id: spawn_config.parent_id.clone(),
                        capability: child_config.capability.clone(),
                        budget: spawn_config.budget.clone(),
                        pipeline: pipeline.clone(),
                    },
                );
                let http_client: Arc<dyn simulacra_http::HttpClient> =
                    Arc::new(simulacra_http::UreqHttpClient::default());
                let mut cell = simulacra_sandbox::AgentCell::new(
                    Arc::clone(&child_proc.vfs),
                    child_config.capability.clone(),
                    Arc::clone(&child_proc.budget),
                    Arc::clone(&child_proc.journal),
                    http_client,
                );
                if let Some(ref executor) = script_executor {
                    cell.set_script_executor(executor.clone());
                }
                if let Some(ref configure_cell) = child_cell_configurator {
                    configure_cell(&mut cell);
                }
                let cell = Arc::new(cell);

                let mut child_registry = simulacra_tool::ToolRegistry::new();
                simulacra_tool::register_builtins(&mut child_registry, Arc::clone(&cell));
                if let Some(ref register_extra_tools) = child_tool_registrar {
                    register_extra_tools(&mut child_registry, Arc::clone(&cell));
                }
                // NO SpawnAgentTool registration — generic agents are leaf workers
                // and cannot spawn children.
                child_proc.tools.set(child_registry.definitions());

                let activity_type = "generic".to_string();
                let child_sink: Arc<dyn ActivitySink> = Arc::new(ForwardingActivitySink::new(
                    spawn_config.agent_id.0.clone(),
                    activity_type,
                    parent_sink,
                ));

                // BEFORE spawn hook
                if let Some(ref pipeline) = pipeline {
                    let before_ctx = serde_json::json!({
                        "agent_type": "generic",
                        "system_prompt": &child_config.system_prompt,
                        "budget": {
                            "max_tokens": spawn_config.budget.max_tokens,
                            "max_turns": spawn_config.budget.max_turns,
                        },
                    })
                    .to_string();
                    match pipeline
                        .run_before(simulacra_hooks::verdict::Operation::Spawn, &before_ctx)
                    {
                        Ok((simulacra_hooks::Verdict::Continue(_), _)) => {}
                        Ok((simulacra_hooks::Verdict::Deny(reason), _)) => {
                            return Err(RuntimeError::HookDenial(reason));
                        }
                        Ok((simulacra_hooks::Verdict::Kill(_), _)) => {
                            unreachable!("Kill is returned as Err from run_before")
                        }
                        Err(simulacra_hooks::HookError::Killed { hook, reason }) => {
                            return Err(RuntimeError::HookKill { hook, reason });
                        }
                        Err(e) => {
                            return Err(RuntimeError::HookError(e.to_string()));
                        }
                    }
                }

                let mut child_loop = AgentLoop::new(
                    child_config,
                    provider,
                    child_registry,
                    Box::new(simulacra_context::ObservationMaskingStrategy::new(5)),
                    child_proc.journal,
                    spawn_config.budget,
                    Some(child_sink),
                    pipeline.clone(),
                );
                child_loop.set_proc_budget_mirror(child_proc.budget, child_proc.turn);

                let result = child_loop.run(&task).await;

                // AFTER spawn hook
                if let Some(ref pipeline) = pipeline {
                    let tokens_used = result.as_ref().map(|o| o.token_usage.total()).unwrap_or(0);
                    let after_ctx = serde_json::json!({
                        "agent_type": "generic",
                        "result": result.as_ref().map(|o| format!("{:?}", o.exit_reason)).unwrap_or_else(|e| format!("{e}")),
                        "tokens_used": tokens_used,
                    })
                    .to_string();
                    let _ =
                        pipeline.run_after(simulacra_hooks::verdict::Operation::Spawn, &after_ctx);
                }

                return result;
            }

            // === CONFIGURED MODE (existing path, agent_type is Some) ===
            let agent_type_config = agent_type_config.ok_or_else(|| {
                RuntimeError::Session(format!(
                    "unknown agent_type: {}",
                    spawn_config.agent_type.as_deref().unwrap_or("<generic>")
                ))
            })?;

            let model = agent_type_config.model.clone();
            let system_prompt = agent_type_config
                .system_prompt
                .clone()
                .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());

            let provider: Box<dyn Provider> = match provider_kind {
                ProviderKind::Anthropic => {
                    let api_key = std::env::var("ANTHROPIC_API_KEY")
                        .map_err(|_| RuntimeError::Session("ANTHROPIC_API_KEY not set".into()))?;
                    Box::new(AnthropicProvider::new(api_key, &model))
                }
                ProviderKind::OpenAI => {
                    let api_key = std::env::var("OPENAI_API_KEY")
                        .map_err(|_| RuntimeError::Session("OPENAI_API_KEY not set".into()))?;
                    Box::new(OpenAiProvider::new(api_key, &model))
                }
                ProviderKind::Ollama => Box::new(OpenAiProvider::new("ollama", &model)),
            };

            let capability = build_capability_token(&agent_type_config);

            // Capability intersection per spec §22:
            // - When spawn_config.capability is Some, three-way: config ∩ override ∩ parent
            // - When None (LLM omitted capabilities field), two-way: config ∩ parent
            //
            // W1: when the override doesn't author memory, inherit the configured
            // agent type's memory before intersecting. The agent_type config is the
            // authoritative source of memory grants for configured spawns; without
            // this, an LLM-supplied capabilities override would strip the configured
            // memory grants by intersecting with default-empty memory.
            let effective_capability = match spawn_config.capability {
                Some(ref override_cap) => {
                    let override_with_memory =
                        inherit_memory_when_override_unset(override_cap, &capability);
                    capability
                        .intersect(&override_with_memory)
                        .intersect(&parent_capability)
                }
                None => capability.intersect(&parent_capability),
            };

            // Check before moving effective_capability into child_config.
            let child_can_spawn = !effective_capability.spawn_types.is_empty();

            let child_config = AgentLoopConfig {
                agent_id: spawn_config.agent_id.clone(),
                system_prompt,
                model,
                max_turns: spawn_config.budget.max_turns,
                capability: effective_capability,
            };

            let agent_type_name = spawn_config
                .agent_type
                .clone()
                .unwrap_or_else(|| "generic".to_string());
            let child_proc = child_proc_runtime(
                Arc::clone(&vfs),
                Arc::clone(&journal),
                ChildProcSpec {
                    agent_id: spawn_config.agent_id.clone(),
                    agent_name: agent_type_name.clone(),
                    model: child_config.model.clone(),
                    parent_id: spawn_config.parent_id.clone(),
                    capability: child_config.capability.clone(),
                    budget: spawn_config.budget.clone(),
                    pipeline: pipeline.clone(),
                },
            );
            let http_client: Arc<dyn simulacra_http::HttpClient> =
                Arc::new(simulacra_http::UreqHttpClient::default());
            let mut cell = simulacra_sandbox::AgentCell::new(
                Arc::clone(&child_proc.vfs),
                child_config.capability.clone(),
                Arc::clone(&child_proc.budget),
                Arc::clone(&child_proc.journal),
                http_client,
            );
            if let Some(ref executor) = script_executor {
                cell.set_script_executor(executor.clone());
            }
            if let Some(ref configure_cell) = child_cell_configurator {
                configure_cell(&mut cell);
            }
            let cell = Arc::new(cell);

            let mut child_registry = simulacra_tool::ToolRegistry::new();
            simulacra_tool::register_builtins(&mut child_registry, Arc::clone(&cell));
            if let Some(ref register_extra_tools) = child_tool_registrar {
                register_extra_tools(&mut child_registry, Arc::clone(&cell));
            }

            // S018 §173: Register spawn_agent for child when it is allowed to spawn.
            if child_can_spawn && let Some(ref sender) = supervisor_sender {
                child_registry.register(Box::new(SpawnAgentTool {
                    sender: sender.clone(),
                    can_spawn: agent_type_config.can_spawn.clone(),
                    activity_sink: Arc::clone(&parent_sink),
                    parent_id: spawn_config.agent_id.clone(),
                    tiers: tiers_config.clone(),
                    // Child's SpawnAgentTool sees the child's own budget so that
                    // grandchildren inherit from the child's remaining budget.
                    parent_budget: Arc::clone(&child_proc.budget),
                    parent_model: child_config.model.clone(),
                }));
            }
            child_proc.tools.set(child_registry.definitions());

            // S019: Create a ForwardingActivitySink that wraps child events in
            // ChildActivity and forwards to the parent's sink for real-time visibility.
            let child_sink: Arc<dyn ActivitySink> = Arc::new(ForwardingActivitySink::new(
                spawn_config.agent_id.0.clone(),
                agent_type_name.clone(),
                parent_sink,
            ));

            // BEFORE spawn hook
            let agent_type_str = agent_type_name;
            if let Some(ref pipeline) = pipeline {
                let before_ctx = serde_json::json!({
                    "agent_type": &agent_type_str,
                    "system_prompt": &child_config.system_prompt,
                    "budget": {
                        "max_tokens": spawn_config.budget.max_tokens,
                        "max_turns": spawn_config.budget.max_turns,
                    },
                })
                .to_string();
                match pipeline.run_before(simulacra_hooks::verdict::Operation::Spawn, &before_ctx) {
                    Ok((simulacra_hooks::Verdict::Continue(_), _)) => {}
                    Ok((simulacra_hooks::Verdict::Deny(reason), _)) => {
                        return Err(RuntimeError::HookDenial(reason));
                    }
                    Ok((simulacra_hooks::Verdict::Kill(_), _)) => {
                        unreachable!("Kill is returned as Err from run_before")
                    }
                    Err(simulacra_hooks::HookError::Killed { hook, reason }) => {
                        return Err(RuntimeError::HookKill { hook, reason });
                    }
                    Err(e) => {
                        return Err(RuntimeError::HookError(e.to_string()));
                    }
                }
            }

            let mut child_loop = AgentLoop::new(
                child_config,
                provider,
                child_registry,
                Box::new(simulacra_context::ObservationMaskingStrategy::new(5)),
                child_proc.journal,
                spawn_config.budget,
                Some(child_sink),
                pipeline.clone(),
            );
            child_loop.set_proc_budget_mirror(child_proc.budget, child_proc.turn);

            let result = child_loop.run(&task).await;

            // AFTER spawn hook
            if let Some(ref pipeline) = pipeline {
                let tokens_used = result.as_ref().map(|o| o.token_usage.total()).unwrap_or(0);
                let after_ctx = serde_json::json!({
                    "agent_type": &agent_type_str,
                    "result": result.as_ref().map(|o| format!("{:?}", o.exit_reason)).unwrap_or_else(|e| format!("{e}")),
                    "tokens_used": tokens_used,
                })
                .to_string();
                let _ = pipeline.run_after(simulacra_hooks::verdict::Operation::Spawn, &after_ctx);
            }

            result
        })
    }
}
