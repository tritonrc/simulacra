use super::*;

// ---------------------------------------------------------------------------
// SpawnAgentTool
// ---------------------------------------------------------------------------

/// Tool that spawns a supervised child agent via the supervisor's mpsc channel.
///
/// When the LLM calls `spawn_agent`, this tool sends a `SupervisorPayload::Spawn`
/// message and returns after the supervisor accepts the child. Terminal results
/// are collected later with `join_child_agent`.
pub struct SpawnAgentTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
    pub can_spawn: Vec<String>,
    /// S019: Activity sink for emitting ChildSpawned/ChildFinished events.
    pub activity_sink: Arc<dyn ActivitySink>,
    /// The parent agent's ID, propagated into SpawnConfig.parent_id.
    pub parent_id: AgentId,
    /// S023: Known tier names from `[tiers]` config. Used for tier validation.
    pub tiers: TierMap,
    /// Parent's budget, used to cap child budgets when the LLM omits or explicitly
    /// requests unlimited (0) budget fields. Without this, "missing" or 0 budget
    /// fields would create unlimited children under a finite-budget parent, which
    /// slips past the supervisor's `child_limit > parent_remaining` check.
    ///
    /// Semantics: when a budget field is absent OR explicitly 0, the child
    /// inherits the parent's **remaining** budget for that resource. When the
    /// parent itself is unlimited (0), the child remains unlimited too.
    pub parent_budget: Arc<Mutex<ResourceBudget>>,
    /// Parent model, used to derive the inherited tier label for generic
    /// children without changing their model-selection fallback.
    pub parent_model: String,
}

impl simulacra_types::Tool for SpawnAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "spawn_agent".to_string(),
            description: "Spawn a supervised child agent to handle a delegated task and return a live child handle.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_type": {
                        "type": "string",
                        "description": "Configured agent type name from simulacra.toml to use for the child agent"
                    },
                    "task": {
                        "type": "string",
                        "description": "The task or instruction delegated to the child agent"
                    },
                    "budget": {
                        "type": "object",
                        "description": "Requested child budget. Each field is an upper bound and must fit within the parent's remaining budget.",
                        "properties": {
                            "max_tokens": { "type": "integer", "minimum": 0 },
                            "max_turns": { "type": "integer", "minimum": 0 },
                            "max_cost": { "type": "string", "description": "Decimal string, same representation as ResourceBudget.max_cost" },
                            "max_sub_agents": { "type": "integer", "minimum": 0 }
                        },
                        "required": ["max_tokens", "max_turns", "max_cost", "max_sub_agents"],
                        "additionalProperties": false
                    },
                    "system_prompt": {
                        "type": "string",
                        "description": "System prompt for generic sub-agent (max 8KB). Required when agent_type is omitted."
                    },
                    "tier": {
                        "type": "string",
                        "description": "Model capability tier. Defaults to parent's tier."
                    },
                    "capabilities": {
                        "type": "object",
                        "description": "Optional attenuated capability override.",
                        "properties": {
                            "network": { "type": "array", "items": { "type": "string" } },
                            "mcp_tools": { "type": "array", "items": { "type": "string" } },
                            "shell": { "type": "boolean" },
                            "javascript": { "type": "boolean" },
                            "python": { "type": "boolean" },
                            "paths_write": { "type": "array", "items": { "type": "string" } },
                            "paths_read": { "type": "array", "items": { "type": "string" } },
                            "spawn_types": { "type": "array", "items": { "type": "string" } }
                        },
                        "additionalProperties": false
                    }
                },
                "required": ["task", "budget"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        let agent_type = arguments
            .get("agent_type")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let system_prompt = arguments
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let tier = arguments
            .get("tier")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let task = arguments
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let caller_spawn_types = capability.spawn_types.clone();

        Box::pin(async move {
            // Validate mutual exclusivity: agent_type XOR system_prompt
            if agent_type.is_some() && system_prompt.is_some() {
                return Err(simulacra_types::ToolError::InvalidArguments(
                    "provide agent_type or system_prompt, not both".into(),
                ));
            }
            if agent_type.is_none() && system_prompt.is_none() {
                return Err(simulacra_types::ToolError::InvalidArguments(
                    "either agent_type or system_prompt is required".into(),
                ));
            }

            // Validate system_prompt size limit (8 KB)
            if let Some(ref sp) = system_prompt
                && sp.len() > 8192
            {
                return Err(simulacra_types::ToolError::ExecutionFailed(format!(
                    "system_prompt exceeds 8192 byte limit (got {} bytes)",
                    sp.len()
                )));
            }

            // S023: Validate tier name against configured tiers
            if let Some(ref t) = tier {
                if self.tiers.is_empty() {
                    tracing::warn!(
                        tier = %t,
                        "tier ignored: no [tiers] config exists, falling back to parent model"
                    );
                } else if !self.tiers.contains_key(t.as_str()) {
                    let valid: Vec<_> = self.tiers.keys().collect();
                    return Err(simulacra_types::ToolError::ExecutionFailed(format!(
                        "unknown tier '{}'. Valid tiers: {:?}",
                        t, valid
                    )));
                }
            }

            // Only check can_spawn for named agent types
            if let Some(ref at) = agent_type
                && !self.can_spawn.contains(at)
            {
                return Err(simulacra_types::ToolError::ExecutionFailed(format!(
                    "agent_type '{}' is not in can_spawn config",
                    at
                )));
            }
            if let Some(ref at) = agent_type
                && !caller_spawn_types.is_empty()
                && !caller_spawn_types.contains(at)
            {
                return Err(simulacra_types::ToolError::ExecutionFailed(format!(
                    "agent_type '{}' is not in caller spawn_types {:?}",
                    at, caller_spawn_types
                )));
            }

            // Parse budget from arguments.
            //
            // BLOCKER 1 fix: when a field is missing or explicitly 0, the child
            // inherits the parent's **remaining** budget for that resource. This
            // is required because 0 means "unlimited" everywhere else in the
            // budget system, so an LLM that omits or zeros a field would have
            // silently created an unlimited child under a finite-budget parent
            // (the supervisor only rejects `child_limit > parent_remaining`,
            // and 0 always passes that check).
            //
            // When the parent itself is unlimited (0), the child's inherited
            // value also stays 0 (unlimited) — this is the only case where 0
            // is allowed to propagate. Explicit positive values from the LLM
            // are kept as-is (the supervisor's headroom check enforces the cap).
            let budget_obj = arguments.get("budget").ok_or_else(|| {
                simulacra_types::ToolError::ExecutionFailed("missing budget".into())
            })?;

            // Snapshot parent's remaining budget under the lock, then release.
            let (
                parent_remaining_tokens,
                parent_remaining_turns,
                parent_remaining_cost,
                parent_remaining_sub_agents,
            ) = {
                let parent = self.parent_budget.lock().map_err(|e| {
                    simulacra_types::ToolError::ExecutionFailed(format!(
                        "parent budget mutex poisoned: {e}"
                    ))
                })?;
                let remaining_tokens = if parent.max_tokens == 0 {
                    0u64 // 0 means unlimited — propagate to child
                } else {
                    parent.max_tokens.saturating_sub(parent.used_tokens)
                };
                let remaining_turns = if parent.max_turns == 0 {
                    0u32
                } else {
                    parent.max_turns.saturating_sub(parent.used_turns)
                };
                let remaining_cost = if parent.max_cost.is_zero() {
                    Decimal::ZERO
                } else {
                    parent.max_cost - parent.used_cost
                };
                let remaining_sub_agents = if parent.max_sub_agents == 0 {
                    0u32
                } else {
                    parent.max_sub_agents.saturating_sub(parent.used_sub_agents)
                };
                (
                    remaining_tokens,
                    remaining_turns,
                    remaining_cost,
                    remaining_sub_agents,
                )
            };

            let parsed_max_tokens = budget_obj.get("max_tokens").and_then(|v| v.as_u64());
            let max_tokens = match parsed_max_tokens {
                Some(n) if n > 0 => n,
                _ => parent_remaining_tokens, // missing OR 0 → inherit parent remaining
            };

            let parsed_max_turns = budget_obj.get("max_turns").and_then(|v| v.as_u64());
            let max_turns = match parsed_max_turns {
                Some(n) if n > 0 => n as u32,
                _ => parent_remaining_turns,
            };

            let parsed_max_cost = budget_obj
                .get("max_cost")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Decimal>().ok());
            let max_cost = match parsed_max_cost {
                Some(c) if !c.is_zero() => c,
                _ => parent_remaining_cost,
            };

            let parsed_max_sub_agents = budget_obj.get("max_sub_agents").and_then(|v| v.as_u64());
            let max_sub_agents = match parsed_max_sub_agents {
                Some(n) if n > 0 => n as u32,
                _ => parent_remaining_sub_agents,
            };

            // Generate child_id: use agent_type name for named agents,
            // "generic" for inline system_prompt agents.
            let child_id = match &agent_type {
                Some(at) => format!(
                    "child-{}-{:016x}",
                    at,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                ),
                None => format!(
                    "child-generic-{:016x}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                ),
            };

            // For the agent_type string used in response/activity events
            let agent_type_label = agent_type.clone().unwrap_or_else(|| "generic".to_string());

            // Parse optional capabilities override from arguments.
            // When the LLM omits the `capabilities` field, capability stays None
            // so the factory uses config ∩ parent (no zeroing).
            let capability = arguments.get("capabilities").map(parse_capability_override);

            let config = SpawnConfig {
                agent_id: AgentId(child_id.clone()),
                parent_id: self.parent_id.clone(),
                capability,
                budget: ResourceBudget::new(max_tokens, max_turns, max_cost, max_sub_agents),
                restart_strategy: crate::RestartStrategy::LetCrash,
                agent_type: agent_type.clone(),
                task: task.clone(),
                system_prompt: system_prompt.clone(),
                tier: tier.clone(),
                resolved_tier: tier.clone().or_else(|| {
                    if agent_type.is_none() {
                        Some(parent_tier_name(&self.tiers, &self.parent_model))
                    } else {
                        None
                    }
                }),
            };

            // Note: ChildSpawned is emitted by the supervisor (spawn_agent),
            // not here, to avoid duplicate emissions.

            let (result_tx, result_rx) = tokio::sync::oneshot::channel();

            let msg = SupervisorMessage {
                agent_id: AgentId(child_id.clone()),
                priority: MessagePriority::Command,
                payload: SupervisorPayload::Spawn(Box::new(config), result_tx),
            };

            self.sender.send(msg).await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
            })?;

            match result_rx.await {
                Ok(Ok(ack)) => Ok(serde_json::json!({
                    "child_id": ack.child_id.0,
                    "agent_type": ack.agent_type,
                    "status": "running"
                })),
                Ok(Err(err)) => Err(simulacra_types::ToolError::ExecutionFailed(format!(
                    "child {child_id} (agent_type={agent_type_label}) failed: {err}"
                ))),
                Err(_) => Err(simulacra_types::ToolError::ExecutionFailed(format!(
                    "child {child_id} (agent_type={agent_type_label}): supervisor dropped spawn acknowledgement channel"
                ))),
            }
        })
    }
}

pub struct JoinChildAgentTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

impl simulacra_types::Tool for JoinChildAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "join_child_agent".to_string(),
            description: "Wait for a live child agent to finish and return its terminal summary."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_id": {
                        "type": "string",
                        "description": "Child agent id returned by spawn_agent"
                    }
                },
                "required": ["child_id"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let child_id = arguments
                .get("child_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    simulacra_types::ToolError::InvalidArguments(
                        "missing required field: child_id".into(),
                    )
                })?
                .to_string();
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            self.sender
                .send(SupervisorMessage {
                    agent_id: AgentId(child_id.clone()),
                    priority: MessagePriority::Command,
                    payload: SupervisorPayload::JoinChild(AgentId(child_id.clone()), result_tx),
                })
                .await
                .map_err(|_| {
                    simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
                })?;
            let terminal = result_rx.await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed(
                    "supervisor dropped join response channel".into(),
                )
            })?;
            let terminal = terminal.map_err(simulacra_types::ToolError::ExecutionFailed)?;
            match terminal.result {
                Ok(output) => Ok(child_success_json(
                    terminal.child_id.0,
                    terminal.agent_type,
                    output,
                )),
                Err(error) => Err(simulacra_types::ToolError::ExecutionFailed(format!(
                    "child {} (agent_type={}) failed: {error}",
                    terminal.child_id.0, terminal.agent_type
                ))),
            }
        })
    }
}

pub struct ChildStatusTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

impl simulacra_types::Tool for ChildStatusTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "child_status".to_string(),
            description: "Inspect the status of a live or completed child agent handle."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_id": {
                        "type": "string",
                        "description": "Child agent id returned by spawn_agent"
                    }
                },
                "required": ["child_id"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let child_id = parse_required_child_id(&arguments)?;
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            self.sender
                .send(SupervisorMessage {
                    agent_id: AgentId(child_id.clone()),
                    priority: MessagePriority::Command,
                    payload: SupervisorPayload::ChildStatus(AgentId(child_id.clone()), result_tx),
                })
                .await
                .map_err(|_| {
                    simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
                })?;
            let status = result_rx.await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed(
                    "supervisor dropped child_status response channel".into(),
                )
            })?;
            let status = status.map_err(simulacra_types::ToolError::ExecutionFailed)?;
            Ok(child_status_json(status))
        })
    }
}

pub struct WaitChildAgentTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

impl simulacra_types::Tool for WaitChildAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wait_child_agent".to_string(),
            description: "Wait for a child agent up to a bounded timeout without consuming its terminal result.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_id": {
                        "type": "string",
                        "description": "Child agent id returned by spawn_agent"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Maximum time to wait in milliseconds. Zero polls once without waiting."
                    }
                },
                "required": ["child_id", "timeout_ms"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let child_id = parse_required_child_id(&arguments)?;
            let timeout_ms = arguments
                .get("timeout_ms")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| {
                    simulacra_types::ToolError::InvalidArguments(
                        "missing or invalid required field: timeout_ms".into(),
                    )
                })?;
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            self.sender
                .send(SupervisorMessage {
                    agent_id: AgentId(child_id.clone()),
                    priority: MessagePriority::Command,
                    payload: SupervisorPayload::WaitChild(
                        AgentId(child_id.clone()),
                        Duration::from_millis(timeout_ms),
                        result_tx,
                    ),
                })
                .await
                .map_err(|_| {
                    simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
                })?;
            let wait = result_rx.await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed(
                    "supervisor dropped wait_child_agent response channel".into(),
                )
            })?;
            let wait = wait.map_err(simulacra_types::ToolError::ExecutionFailed)?;
            Ok(wait_child_json(wait))
        })
    }
}

pub struct CloseChildAgentTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

impl simulacra_types::Tool for CloseChildAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "close_child_agent".to_string(),
            description: "Release a completed child agent handle and its cached terminal result."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_id": {
                        "type": "string",
                        "description": "Child agent id returned by spawn_agent"
                    }
                },
                "required": ["child_id"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let child_id = parse_required_child_id(&arguments)?;
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            self.sender
                .send(SupervisorMessage {
                    agent_id: AgentId(child_id.clone()),
                    priority: MessagePriority::Command,
                    payload: SupervisorPayload::CloseChild(AgentId(child_id.clone()), result_tx),
                })
                .await
                .map_err(|_| {
                    simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
                })?;
            let result = result_rx.await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed(
                    "supervisor dropped close_child_agent response channel".into(),
                )
            })?;
            result.map_err(simulacra_types::ToolError::ExecutionFailed)?;
            Ok(serde_json::json!({
                "child_id": child_id,
                "status": "closed"
            }))
        })
    }
}

pub struct SteerChildAgentTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

impl simulacra_types::Tool for SteerChildAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "steer_child_agent".to_string(),
            description:
                "Queue additional instructions for a live child agent before its next model turn."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_id": {
                        "type": "string",
                        "description": "Child agent id returned by spawn_agent"
                    },
                    "message": {
                        "type": "string",
                        "description": "Additional instruction to queue for the child agent"
                    }
                },
                "required": ["child_id", "message"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let child_id = arguments
                .get("child_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    simulacra_types::ToolError::InvalidArguments(
                        "missing required field: child_id".into(),
                    )
                })?
                .to_string();
            let message = arguments
                .get("message")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    simulacra_types::ToolError::InvalidArguments(
                        "missing required field: message".into(),
                    )
                })?
                .to_string();
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            self.sender
                .send(SupervisorMessage {
                    agent_id: AgentId(child_id.clone()),
                    priority: MessagePriority::Command,
                    payload: SupervisorPayload::SteerChild(
                        AgentId(child_id.clone()),
                        message,
                        result_tx,
                    ),
                })
                .await
                .map_err(|_| {
                    simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
                })?;
            let result = result_rx.await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed(
                    "supervisor dropped steer response channel".into(),
                )
            })?;
            result.map_err(simulacra_types::ToolError::ExecutionFailed)?;
            Ok(serde_json::json!({
                "child_id": child_id,
                "status": "queued"
            }))
        })
    }
}

pub struct CancelChildAgentTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
}

impl simulacra_types::Tool for CancelChildAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "cancel_child_agent".to_string(),
            description: "Request cancellation for a live child agent.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "child_id": {
                        "type": "string",
                        "description": "Child agent id returned by spawn_agent"
                    },
                    "reason": {
                        "type": "string",
                        "description": "Optional cancellation reason"
                    }
                },
                "required": ["child_id"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let child_id = arguments
                .get("child_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    simulacra_types::ToolError::InvalidArguments(
                        "missing required field: child_id".into(),
                    )
                })?
                .to_string();
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            self.sender
                .send(SupervisorMessage {
                    agent_id: AgentId(child_id.clone()),
                    priority: MessagePriority::Signal,
                    payload: SupervisorPayload::CancelChild(AgentId(child_id.clone()), result_tx),
                })
                .await
                .map_err(|_| {
                    simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
                })?;
            let result = result_rx.await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed(
                    "supervisor dropped cancel response channel".into(),
                )
            })?;
            result.map_err(simulacra_types::ToolError::ExecutionFailed)?;
            Ok(serde_json::json!({
                "child_id": child_id,
                "status": "cancel_requested"
            }))
        })
    }
}

fn parse_required_child_id(arguments: &serde_json::Value) -> Result<String, ToolError> {
    arguments
        .get("child_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| ToolError::InvalidArguments("missing required field: child_id".into()))
}

fn child_success_json(
    child_id: String,
    agent_type: String,
    output: AgentLoopOutput,
) -> serde_json::Value {
    let exit_reason_str = exit_reason_to_snake_case(&output.exit_reason);
    let message = output
        .messages
        .last()
        .filter(|m| m.role == simulacra_types::Role::Assistant)
        .map(|m| m.content.clone())
        .unwrap_or_default();

    serde_json::json!({
        "child_id": child_id,
        "agent_type": agent_type,
        "exit_reason": exit_reason_str,
        "message": message,
        "token_usage": {
            "input_tokens": output.token_usage.input_tokens,
            "output_tokens": output.token_usage.output_tokens
        }
    })
}

fn child_status_json(status: ChildStatus) -> serde_json::Value {
    serde_json::json!({
        "child_id": status.child_id.0,
        "agent_type": status.agent_type,
        "status": status.status,
        "ready": status.ready,
        "elapsed_ms": status.elapsed_ms
    })
}

fn wait_child_json(wait: WaitChildResult) -> serde_json::Value {
    if let Some(terminal) = wait.terminal {
        match terminal.result {
            Ok(output) => {
                let mut json = child_success_json(terminal.child_id.0, terminal.agent_type, output);
                if let serde_json::Value::Object(ref mut object) = json {
                    object.insert("status".to_string(), serde_json::Value::String(wait.status));
                    object.insert("ready".to_string(), serde_json::Value::Bool(true));
                }
                json
            }
            Err(error) => serde_json::json!({
                "child_id": terminal.child_id.0,
                "agent_type": terminal.agent_type,
                "status": wait.status,
                "ready": true,
                "exit_reason": "failed",
                "message": error,
                "token_usage": {
                    "input_tokens": 0,
                    "output_tokens": 0
                }
            }),
        }
    } else {
        serde_json::json!({
            "child_id": wait.child_id.0,
            "status": "running",
            "ready": false
        })
    }
}
