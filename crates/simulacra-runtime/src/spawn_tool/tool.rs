use super::*;

const SPAWN_AGENT_DESCRIPTION: &str = "I can start a supervised child for one concrete, bounded, independent task. Choose where I run it with placement and shape how it works with instructions; placement supplies an environment and capabilities, not a role. I return a live handle, not the child's final answer.";
const PLACEMENT_DESCRIPTION_PREFIX: &str = "Where I should run this child and which host-supplied capability envelope it receives. This selects placement, not a role.";
const INSTRUCTIONS_DESCRIPTION: &str = "How I should shape this child for the delegated task, including any relevant available skills and evidence requirements. This does not grant capabilities.";
const TASK_DESCRIPTION: &str = "The concrete, bounded work I should hand to the child.";
const BUDGET_DESCRIPTION: &str = "The maximum resources I should reserve for this child; each nonzero value must fit within my remaining budget and the placement limits, while zero requests unlimited capacity under the rules below.";
const CAPABILITIES_DESCRIPTION: &str = "Capabilities I should remove from this child's placement envelope; these values can only attenuate access.";
const MAX_INSTRUCTION_BYTES: usize = 65_536;

static NEXT_CHILD_ID: AtomicU64 = AtomicU64::new(1);

/// Host-provided, model-visible guidance for the `spawn_agent` contract.
pub struct SpawnAgentGuidance {
    pub description: String,
    pub result_note: Option<String>,
}

/// Starts one placement-backed child through the supervisor boundary.
pub struct SpawnAgentTool {
    pub sender: tokio::sync::mpsc::Sender<SupervisorMessage>,
    pub allowed_placements: Vec<String>,
    pub activity_sink: Arc<dyn ActivitySink>,
    pub parent_id: AgentId,
    pub parent_budget: Arc<Mutex<ResourceBudget>>,
    pub guidance: Option<SpawnAgentGuidance>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArguments {
    placement: String,
    #[serde(default)]
    instructions: Option<String>,
    task: String,
    budget: SpawnBudget,
    #[serde(default)]
    capabilities: Option<SpawnCapabilities>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnBudget {
    max_tokens: u64,
    max_turns: u32,
    max_cost: String,
    max_sub_agents: u32,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnCapabilities {
    #[serde(default)]
    network: Option<Vec<String>>,
    #[serde(default)]
    mcp_tools: Option<Vec<String>>,
    #[serde(default)]
    shell: Option<bool>,
    #[serde(default)]
    javascript: Option<bool>,
    #[serde(default)]
    python: Option<bool>,
    #[serde(default)]
    paths_write: Option<Vec<String>>,
    #[serde(default)]
    paths_read: Option<Vec<String>>,
    #[serde(default)]
    spawn_placements: Option<Vec<String>>,
}

impl SpawnCapabilities {
    fn into_token(self, parent: &CapabilityToken) -> CapabilityToken {
        let requested = CapabilityToken {
            network: self.network.map_or_else(
                || parent.network.clone(),
                |values| values.into_iter().map(NetworkPermission).collect(),
            ),
            mcp_tools: self.mcp_tools.unwrap_or_else(|| parent.mcp_tools.clone()),
            shell: self.shell.unwrap_or(parent.shell),
            javascript: self.javascript.unwrap_or(parent.javascript),
            python: self.python.unwrap_or(parent.python),
            paths_write: self.paths_write.map_or_else(
                || parent.paths_write.clone(),
                |values| {
                    values
                        .into_iter()
                        .map(|value| normalize_spawn_path_scope(&value))
                        .collect()
                },
            ),
            paths_read: self.paths_read.map_or_else(
                || parent.paths_read.clone(),
                |values| {
                    values
                        .into_iter()
                        .map(|value| normalize_spawn_path_scope(&value))
                        .collect()
                },
            ),
            spawn_placements: self
                .spawn_placements
                .unwrap_or_else(|| parent.spawn_placements.clone()),
            skill_patterns: parent.skill_patterns.clone(),
            memory: parent.memory.clone(),
        };
        parent.intersect(&requested)
    }
}

impl simulacra_types::Tool for SpawnAgentTool {
    fn definition(&self) -> ToolDefinition {
        let mut placements = self.allowed_placements.clone();
        placements.sort();
        placements.dedup();
        let placement_description = if placements.is_empty() {
            format!(
                "{PLACEMENT_DESCRIPTION_PREFIX} No child placements are available in this session."
            )
        } else {
            let quoted = placements
                .iter()
                .filter_map(|placement| serde_json::to_string(placement).ok())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{PLACEMENT_DESCRIPTION_PREFIX} Available placements: {quoted}.")
        };

        ToolDefinition {
            name: "spawn_agent".to_string(),
            description: self.guidance.as_ref().map_or_else(
                || SPAWN_AGENT_DESCRIPTION.to_string(),
                |guidance| guidance.description.clone(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "placement": {
                        "type": "string",
                        "description": placement_description
                    },
                    "instructions": {
                        "type": "string",
                        "description": INSTRUCTIONS_DESCRIPTION
                    },
                    "task": {
                        "type": "string",
                        "description": TASK_DESCRIPTION
                    },
                    "budget": {
                        "type": "object",
                        "description": BUDGET_DESCRIPTION,
                        "properties": {
                            "max_tokens": { "type": "integer", "minimum": 0 },
                            "max_turns": { "type": "integer", "minimum": 0 },
                            "max_cost": { "type": "string", "description": "The decimal cost limit I should reserve, represented as a string." },
                            "max_sub_agents": { "type": "integer", "minimum": 0 }
                        },
                        "required": ["max_tokens", "max_turns", "max_cost", "max_sub_agents"],
                        "additionalProperties": false
                    },
                    "capabilities": {
                        "type": "object",
                        "description": CAPABILITIES_DESCRIPTION,
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
                    }
                },
                "required": ["placement", "task", "budget"],
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
        let caller_spawn_placements = capability.spawn_placements.clone();
        let caller_capability = capability.clone();
        Box::pin(async move {
            validate_argument_shape(&arguments)?;
            let parsed: SpawnArguments = serde_json::from_value(arguments)
                .map_err(|error| simulacra_types::ToolError::InvalidArguments(error.to_string()))?;
            validate_text(&parsed.placement, "placement")?;
            validate_text(&parsed.task, "task")?;

            let instructions = match parsed.instructions {
                Some(value) => {
                    if value.len() > MAX_INSTRUCTION_BYTES {
                        return Err(simulacra_types::ToolError::InvalidArguments(format!(
                            "instructions has {} UTF-8 bytes; maximum is {MAX_INSTRUCTION_BYTES}",
                            value.len()
                        )));
                    }
                    (!value.trim().is_empty()).then_some(value)
                }
                None => None,
            };

            let mut available = self.allowed_placements.clone();
            available.sort();
            available.dedup();
            if !available.contains(&parsed.placement) {
                let suffix = if available.is_empty() {
                    String::new()
                } else {
                    format!("; available placements: {}", available.join(", "))
                };
                return Err(simulacra_types::ToolError::InvalidArguments(format!(
                    "unknown or unauthorized placement {:?}{suffix}",
                    parsed.placement
                )));
            }
            if !caller_spawn_placements.contains(&parsed.placement) {
                return Err(simulacra_types::ToolError::InvalidArguments(format!(
                    "placement {:?} is not authorized by caller spawn_placements",
                    parsed.placement
                )));
            }

            let max_cost = parsed.budget.max_cost.parse::<Decimal>().map_err(|_| {
                simulacra_types::ToolError::InvalidArguments(format!(
                    "max_cost {:?} is not a nonnegative decimal string",
                    parsed.budget.max_cost
                ))
            })?;
            if max_cost.is_sign_negative() {
                return Err(simulacra_types::ToolError::InvalidArguments(format!(
                    "max_cost {:?} is not a nonnegative decimal string",
                    parsed.budget.max_cost
                )));
            }
            let requested = ResourceBudget::new(
                parsed.budget.max_tokens,
                parsed.budget.max_turns,
                max_cost,
                parsed.budget.max_sub_agents,
            );
            {
                let parent = self.parent_budget.lock().map_err(|error| {
                    simulacra_types::ToolError::ExecutionFailed(format!(
                        "parent budget mutex poisoned: {error}"
                    ))
                })?;
                validate_parent_budget(&requested, &parent)?;
            }

            let child_id = next_child_id();
            let config = SpawnConfig {
                agent_id: AgentId(child_id.clone()),
                parent_id: self.parent_id.clone(),
                capability: parsed
                    .capabilities
                    .map(|override_capability| override_capability.into_token(&caller_capability)),
                budget: requested,
                restart_strategy: crate::RestartStrategy::LetCrash,
                placement: parsed.placement.clone(),
                task: parsed.task,
                instructions,
            };
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            crate::supervisor::register_spawn_parent_span(
                &config.parent_id,
                &config.agent_id,
                tracing::Span::current(),
            );
            if self
                .sender
                .send(SupervisorMessage {
                    agent_id: self.parent_id.clone(),
                    priority: MessagePriority::Command,
                    payload: SupervisorPayload::Spawn(Box::new(config), result_tx),
                })
                .await
                .is_err()
            {
                let _ =
                    crate::supervisor::take_spawn_parent_span(&self.parent_id, &AgentId(child_id));
                return Err(simulacra_types::ToolError::ExecutionFailed(
                    "supervisor channel closed".into(),
                ));
            }

            let result = result_rx.await;
            // The real supervisor takes this context before accepting the
            // spawn. Fake receivers and rejected/dropped requests leave it for
            // the caller to clean up here.
            let _ = crate::supervisor::take_spawn_parent_span(
                &self.parent_id,
                &AgentId(child_id.clone()),
            );
            match result {
                Ok(Ok(ack)) => {
                    let mut acknowledgement = serde_json::json!({
                        "child_id": ack.child_id.0,
                        "placement": ack.placement,
                        "status": "running"
                    });
                    if let (Some(object), Some(note)) = (
                        acknowledgement.as_object_mut(),
                        self.guidance
                            .as_ref()
                            .and_then(|guidance| guidance.result_note.as_ref()),
                    ) {
                        object.insert("note".to_string(), serde_json::Value::String(note.clone()));
                    }
                    Ok(acknowledgement)
                }
                Ok(Err(error)) => Err(simulacra_types::ToolError::ExecutionFailed(format!(
                    "child {child_id} (placement={:?}) failed: {error}",
                    parsed.placement
                ))),
                Err(_) => Err(simulacra_types::ToolError::ExecutionFailed(format!(
                    "child {child_id} (placement={:?}): supervisor dropped spawn acknowledgement channel",
                    parsed.placement
                ))),
            }
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }
}

fn validate_argument_shape(
    arguments: &serde_json::Value,
) -> Result<(), simulacra_types::ToolError> {
    let object = arguments.as_object().ok_or_else(|| {
        simulacra_types::ToolError::InvalidArguments(
            "spawn_agent arguments must be an object".into(),
        )
    })?;
    const TOP_LEVEL: &[&str] = &[
        "placement",
        "instructions",
        "task",
        "budget",
        "capabilities",
    ];
    reject_unknown_keys(object, TOP_LEVEL)?;
    require_string(object, "placement")?;
    require_string(object, "task")?;
    if object
        .get("instructions")
        .is_some_and(|value| !value.is_string())
    {
        return invalid_field("instructions", "must be a string");
    }

    let budget = object
        .get("budget")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            simulacra_types::ToolError::InvalidArguments("budget must be an object".into())
        })?;
    const BUDGET: &[&str] = &["max_tokens", "max_turns", "max_cost", "max_sub_agents"];
    reject_unknown_keys(budget, BUDGET)?;
    require_u64(budget, "max_tokens", u64::MAX)?;
    require_u64(budget, "max_turns", u64::from(u32::MAX))?;
    require_string(budget, "max_cost")?;
    require_u64(budget, "max_sub_agents", u64::from(u32::MAX))?;

    if let Some(capabilities) = object.get("capabilities") {
        let capabilities = capabilities.as_object().ok_or_else(|| {
            simulacra_types::ToolError::InvalidArguments("capabilities must be an object".into())
        })?;
        const CAPABILITIES: &[&str] = &[
            "network",
            "mcp_tools",
            "shell",
            "javascript",
            "python",
            "paths_write",
            "paths_read",
            "spawn_placements",
        ];
        reject_unknown_keys(capabilities, CAPABILITIES)?;
        for field in [
            "network",
            "mcp_tools",
            "paths_write",
            "paths_read",
            "spawn_placements",
        ] {
            if let Some(value) = capabilities.get(field)
                && !value
                    .as_array()
                    .is_some_and(|items| items.iter().all(serde_json::Value::is_string))
            {
                return invalid_field(field, "must be an array of strings");
            }
        }
        for field in ["shell", "javascript", "python"] {
            if capabilities
                .get(field)
                .is_some_and(|value| !value.is_boolean())
            {
                return invalid_field(field, "must be a boolean");
            }
        }
    }
    Ok(())
}

fn reject_unknown_keys(
    object: &serde_json::Map<String, serde_json::Value>,
    allowed: &[&str],
) -> Result<(), simulacra_types::ToolError> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return invalid_field(key, "is unknown");
    }
    Ok(())
}

fn require_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<(), simulacra_types::ToolError> {
    if !object.get(field).is_some_and(serde_json::Value::is_string) {
        return invalid_field(field, "is required and must be a string");
    }
    Ok(())
}

fn require_u64(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    maximum: u64,
) -> Result<(), simulacra_types::ToolError> {
    if !object
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|value| value <= maximum)
    {
        return invalid_field(
            field,
            "is required and must be a nonnegative integer in range",
        );
    }
    Ok(())
}

fn invalid_field<T>(field: &str, message: &str) -> Result<T, simulacra_types::ToolError> {
    Err(simulacra_types::ToolError::InvalidArguments(format!(
        "{field} {message}"
    )))
}

fn validate_text(value: &str, field: &str) -> Result<(), simulacra_types::ToolError> {
    if value.trim().is_empty() {
        return Err(simulacra_types::ToolError::InvalidArguments(format!(
            "{field} must be a non-blank string"
        )));
    }
    Ok(())
}

fn validate_parent_budget(
    requested: &ResourceBudget,
    parent: &ResourceBudget,
) -> Result<(), simulacra_types::ToolError> {
    validate_limit(
        "max_tokens",
        requested.max_tokens,
        parent.max_tokens,
        parent.max_tokens.saturating_sub(parent.used_tokens),
    )?;
    validate_limit(
        "max_turns",
        requested.max_turns,
        parent.max_turns,
        parent.max_turns.saturating_sub(parent.used_turns),
    )?;
    validate_decimal_limit(
        "max_cost",
        requested.max_cost,
        parent.max_cost,
        parent.max_cost - parent.used_cost.min(parent.max_cost),
    )?;
    validate_limit(
        "max_sub_agents",
        requested.max_sub_agents,
        parent.max_sub_agents,
        parent.max_sub_agents.saturating_sub(parent.used_sub_agents),
    )
}

fn validate_limit<T>(
    field: &str,
    requested: T,
    parent_maximum: T,
    parent_remaining: T,
) -> Result<(), simulacra_types::ToolError>
where
    T: Copy + Default + PartialEq + PartialOrd + std::fmt::Display,
{
    if requested == T::default() && parent_maximum != T::default() {
        return Err(simulacra_types::ToolError::InvalidArguments(format!(
            "{field} requested {requested} (unlimited), but parent remaining is {parent_remaining}"
        )));
    }
    if requested != T::default() && parent_maximum != T::default() && requested > parent_remaining {
        return Err(simulacra_types::ToolError::InvalidArguments(format!(
            "{field} requested {requested} exceeds parent remaining {parent_remaining}"
        )));
    }
    Ok(())
}

fn validate_decimal_limit(
    field: &str,
    requested: Decimal,
    parent_maximum: Decimal,
    parent_remaining: Decimal,
) -> Result<(), simulacra_types::ToolError> {
    validate_limit(field, requested, parent_maximum, parent_remaining)
}

fn next_child_id() -> String {
    let counter = NEXT_CHILD_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    format!("child-{nanos:016x}{counter:016x}")
}
