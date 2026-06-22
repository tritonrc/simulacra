//! SpawnAgentTool and AgentTaskFactory — moved from simulacra-cli.
//!
//! `SpawnAgentTool` is a `Tool` implementation that sends spawn requests to the
//! supervisor via an mpsc channel. `AgentTaskFactory` is the `TaskFactory`
//! implementation that constructs child `AgentLoop` instances.

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rust_decimal::Decimal;
use simulacra_config::{SimulacraConfig, TierMap, build_capability_token};
use simulacra_provider::{AnthropicProvider, OpenAiProvider};
use simulacra_types::{
    ActivityEvent, AgentId, CapabilityToken, ContextStrategy, ExitReason, JournalStorage, Message,
    NetworkPermission, PathPattern, Provider, ResourceBudget, ToolDefinition, VirtualFs,
};
use simulacra_vfs::{HookLister, ProcFs, ProcState, ToolLister};

use crate::{
    ActivitySink, AgentLoop, AgentLoopConfig, BoxTaskFuture, CancellationToken,
    CountingJournalStorage, ForwardingActivitySink, MessagePriority, RuntimeError, SpawnConfig,
    SupervisorMessage, SupervisorPayload,
};

// ---------------------------------------------------------------------------
// DEFAULT_SYSTEM_PROMPT
// ---------------------------------------------------------------------------

/// Default system prompt used for child agents when no explicit prompt is
/// configured in the agent type.
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are a helpful AI assistant running inside Simulacra, a sandboxed agent runtime.

You have access to these tools:
- **js_exec**: Execute JavaScript (ESM, QuickJS engine). Each call gets a fresh \
JS global/context, so globals, prototypes, and module singletons do not persist between calls. \
Use `import` not `require`. Available modules: `simulacra:fs`/`fs` (readFileSync, \
writeFileSync, appendFileSync, readdirSync, statSync, renameSync, unlinkSync, mkdirSync), \
`simulacra:console`, `simulacra:process`, `simulacra:path`, and `simulacra:crypto`.
- **shell_exec**: Execute shell commands in a sandboxed emulator. Supports builtins \
(`echo`, `cat`, `ls`, `mkdir`, `cp`, `mv`, `rm`, `pwd`, `env`, `which`, `export`, `grep`, \
`head`, `tail`, `sed`, `wc`, `find`, `sort`, `uniq`, `cut`, `tr`, `tee`, `curl`, `wget`) \
plus pipes, redirects, `&&`, `||`, and `;`. Cwd and env vars persist across shell calls. \
`node <file.js>`, `node -e <code>`, `node -`, `python <script.py>`, `python -c <code>`, \
and `python -` run through the sandboxed JS/Python engines when capabilities allow.
- **file_read**, **file_write**, **file_edit**: Read, write, or edit files in the virtual filesystem.
- **list_dir**: List directory contents.

All file paths are relative to `/workspace/`. Network access is available when permitted by \
the agent's capability token — use `curl` or `wget` for HTTP requests, or `fetch()` in JavaScript. \
For computation, prefer writing pure JavaScript (no imports needed for math/string/array operations) \
and use `console.log()` for output. Write durable artifacts to `/proc/mailbox/<filename>`.";

#[derive(Clone, Default)]
struct RuntimeSharedToolList(Arc<Mutex<Vec<ToolDefinition>>>);

impl RuntimeSharedToolList {
    fn set(&self, definitions: Vec<ToolDefinition>) {
        *self
            .0
            .lock()
            .expect("tool definition list lock should not be poisoned") = definitions;
    }
}

impl ToolLister for RuntimeSharedToolList {
    fn tool_names(&self) -> Vec<String> {
        self.0
            .lock()
            .expect("tool definition list lock should not be poisoned")
            .iter()
            .map(|definition| definition.name.clone())
            .collect()
    }

    fn tool_json(&self, name: &str) -> Option<String> {
        self.0
            .lock()
            .expect("tool definition list lock should not be poisoned")
            .iter()
            .find(|definition| definition.name == name)
            .and_then(|definition| serde_json::to_string(definition).ok())
    }
}

struct RuntimePipelineHookLister(Option<Arc<simulacra_hooks::pipeline::HookPipeline>>);

impl HookLister for RuntimePipelineHookLister {
    fn hook_names(&self, operation: &str) -> Vec<String> {
        let Some(pipeline) = self.0.as_ref() else {
            return vec![];
        };
        use simulacra_hooks::verdict::Operation;
        let operation = match operation {
            "tool_call" => Operation::ToolCall,
            "llm" => Operation::Llm,
            "spawn" => Operation::Spawn,
            "http_request" => Operation::HttpRequest,
            "vfs_write" => Operation::VfsWrite,
            _ => return vec![],
        };
        pipeline.hook_names(operation)
    }
}

struct ChildProcRuntime {
    vfs: Arc<dyn VirtualFs>,
    journal: Arc<dyn JournalStorage>,
    budget: Arc<Mutex<ResourceBudget>>,
    turn: Arc<AtomicU64>,
    tools: RuntimeSharedToolList,
}

struct ChildProcSpec {
    agent_id: AgentId,
    agent_name: String,
    model: String,
    parent_id: AgentId,
    capability: CapabilityToken,
    budget: ResourceBudget,
    pipeline: Option<Arc<simulacra_hooks::pipeline::HookPipeline>>,
}

fn child_proc_runtime(
    inherited_vfs: Arc<dyn VirtualFs>,
    inherited_journal: Arc<dyn JournalStorage>,
    spec: ChildProcSpec,
) -> ChildProcRuntime {
    let budget = Arc::new(Mutex::new(spec.budget));
    let turn = Arc::new(AtomicU64::new(0));
    let journal_entries = Arc::new(AtomicU64::new(0));
    let tools = RuntimeSharedToolList::default();
    let state = Arc::new(ProcState {
        agent_id: spec.agent_id.0.clone(),
        agent_name: spec.agent_name,
        model: spec.model,
        parent_id: Some(spec.parent_id.0),
        budget: Arc::clone(&budget),
        capabilities: spec.capability,
        tools: Arc::new(tools.clone()),
        session_id: spec.agent_id.0,
        session_start: Instant::now(),
        journal_entries: Arc::clone(&journal_entries),
        hooks: Arc::new(RuntimePipelineHookLister(spec.pipeline)),
        turn: Arc::clone(&turn),
    });
    let vfs: Arc<dyn VirtualFs> = Arc::new(ProcFs::new(inherited_vfs, state));
    let journal: Arc<dyn JournalStorage> = Arc::new(CountingJournalStorage::new(
        inherited_journal,
        Arc::clone(&journal_entries),
    ));

    ChildProcRuntime {
        vfs,
        journal,
        budget,
        turn,
        tools,
    }
}

// ---------------------------------------------------------------------------
// ProviderKind
// ---------------------------------------------------------------------------

/// Which LLM provider backend to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    OpenAI,
    Ollama,
}

// ---------------------------------------------------------------------------
// NoopContextStrategy
// ---------------------------------------------------------------------------

/// A context strategy that performs no compaction — returns messages as-is.
pub struct NoopContextStrategy;

impl ContextStrategy for NoopContextStrategy {
    fn compact(&self, messages: &[Message], _token_limit: u64) -> Vec<Message> {
        messages.to_vec()
    }
}

// ---------------------------------------------------------------------------
// SpawnAgentTool
// ---------------------------------------------------------------------------

/// Tool that spawns a supervised child agent via the supervisor's mpsc channel.
///
/// When the LLM calls `spawn_agent`, this tool sends a `SupervisorPayload::Spawn`
/// message and awaits the result via a oneshot channel. The call is synchronous
/// from the parent's perspective (S018). The child's system_prompt is resolved
/// from the agent_type config (e.g. a "researcher" agent_type in simulacra.toml).
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

/// Convert an `ExitReason` to a snake_case string per spec.
pub(crate) fn exit_reason_to_snake_case(reason: &ExitReason) -> String {
    match reason {
        ExitReason::Complete => "completed".into(),
        ExitReason::MaxTurns => "max_turns".into(),
        ExitReason::BudgetExhausted => "budget_exhausted".into(),
        ExitReason::GuardrailTripped(s) => format!("guardrail_tripped:{s}"),
        ExitReason::AwaitingApproval => "awaiting_approval".into(),
        ExitReason::Cancelled => "cancelled".into(),
        ExitReason::PolicyKill { hook, reason } => {
            format!("policy_kill:{hook}:{reason}")
        }
        ExitReason::Error(s) => format!("error:{s}"),
    }
}

/// Parse an optional `capabilities` JSON object into a `CapabilityToken`.
///
/// **Note on memory:** the `capabilities` JSON object does not currently
/// expose a `memory` field — there is no way for an LLM to ask for or
/// narrow memory grants at spawn time. Per W1 from the S037 capability
/// sandbox review, the factory call sites that intersect this override
/// against the parent's capabilities MUST inherit `parent.memory` rather
/// than using the parsed override's default-empty `MemoryCapability`,
/// otherwise children would silently lose memory access whenever a
/// capability override is supplied. See `inherit_memory_when_override_unset`.
fn parse_capability_override(value: &serde_json::Value) -> CapabilityToken {
    let network = value
        .get("network")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| NetworkPermission(s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let mcp_tools = value
        .get("mcp_tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let shell = value
        .get("shell")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let javascript = value
        .get("javascript")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let python = value
        .get("python")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let paths_write = value
        .get("paths_write")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| PathPattern(s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let paths_read = value
        .get("paths_read")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| PathPattern(s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let spawn_types = value
        .get("spawn_types")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    CapabilityToken {
        network,
        mcp_tools,
        shell,
        javascript,
        python,
        paths_write,
        paths_read,
        spawn_types,
        skill_patterns: vec![],
        // Memory is intentionally left at default here. The factory call
        // sites use `inherit_memory_when_override_unset` below to copy
        // parent.memory into the override before intersecting, so an
        // unmentioned memory grant inherits rather than being stripped.
        memory: simulacra_types::MemoryCapability::default(),
    }
}

/// W1 fix: an override parsed from spawn_agent JSON has no way to specify
/// `memory`, so the parsed token always carries `MemoryCapability::default()`
/// (disabled, empty scopes). Intersecting that against the parent would
/// silently strip the parent's memory grants from the child, which is the
/// opposite of what "the LLM did not mention memory" should mean.
///
/// This helper detects "the override's memory is the unset default" and, in
/// that case, copies the parent's memory into the override before intersect.
/// When the JSON capabilities object grows a `memory` field in the future,
/// this helper should be replaced with explicit tracking of whether the
/// override authored memory.
fn inherit_memory_when_override_unset(
    override_cap: &CapabilityToken,
    parent: &CapabilityToken,
) -> CapabilityToken {
    let mut out = override_cap.clone();
    if out.memory == simulacra_types::MemoryCapability::default() {
        out.memory = parent.memory.clone();
    }
    out
}

impl simulacra_types::Tool for SpawnAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "spawn_agent".to_string(),
            description: "Spawn a supervised child agent to handle a delegated task and return its terminal summary.".to_string(),
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
        _capability: &CapabilityToken,
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

            let spawn_start = std::time::Instant::now();
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();

            let msg = SupervisorMessage {
                agent_id: AgentId(child_id.clone()),
                priority: MessagePriority::Command,
                payload: SupervisorPayload::Spawn(Box::new(config), result_tx),
            };

            self.sender.send(msg).await.map_err(|_| {
                simulacra_types::ToolError::ExecutionFailed("supervisor channel closed".into())
            })?;

            // Wait for the child to complete
            match result_rx.await {
                Ok(Ok(output)) => {
                    let duration_ms = spawn_start.elapsed().as_millis() as u64;
                    let tool_uses = output.used_turns;
                    let token_count = output.token_usage.total();

                    let exit_reason_str = exit_reason_to_snake_case(&output.exit_reason);

                    // S019: Emit ActivityEvent::ChildFinished with aggregated stats
                    self.activity_sink.emit(ActivityEvent::ChildFinished {
                        child_id: child_id.clone(),
                        agent_type: agent_type_label.clone(),
                        exit_reason: exit_reason_str.clone(),
                        duration_ms,
                        tool_uses,
                        token_count,
                    });

                    let message = output
                        .messages
                        .last()
                        .filter(|m| m.role == simulacra_types::Role::Assistant)
                        .map(|m| m.content.clone())
                        .unwrap_or_default();

                    Ok(serde_json::json!({
                        "child_id": child_id,
                        "agent_type": agent_type_label,
                        "exit_reason": exit_reason_str,
                        "message": message,
                        "token_usage": {
                            "input_tokens": output.token_usage.input_tokens,
                            "output_tokens": output.token_usage.output_tokens
                        }
                    }))
                }
                Ok(Err(err)) => {
                    let duration_ms = spawn_start.elapsed().as_millis() as u64;
                    // S019: Emit ChildFinished on error too
                    self.activity_sink.emit(ActivityEvent::ChildFinished {
                        child_id: child_id.clone(),
                        agent_type: agent_type_label.clone(),
                        exit_reason: format!("Error: {err}"),
                        duration_ms,
                        tool_uses: 0,
                        token_count: 0,
                    });
                    Err(simulacra_types::ToolError::ExecutionFailed(format!(
                        "child {child_id} (agent_type={agent_type_label}) failed: {err}"
                    )))
                }
                Err(_) => {
                    let duration_ms = spawn_start.elapsed().as_millis() as u64;
                    self.activity_sink.emit(ActivityEvent::ChildFinished {
                        child_id: child_id.clone(),
                        agent_type: agent_type_label.clone(),
                        exit_reason: "supervisor dropped result channel".into(),
                        duration_ms,
                        tool_uses: 0,
                        token_count: 0,
                    });
                    Err(simulacra_types::ToolError::ExecutionFailed(format!(
                        "child {child_id} (agent_type={agent_type_label}): supervisor dropped result channel"
                    )))
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// resolve_tier_model
// ---------------------------------------------------------------------------

/// Resolve a model name from a tier name, falling back to the parent's model.
fn resolve_tier_model(tier: Option<&str>, tiers_config: &TierMap, parent_model: &str) -> String {
    match tier {
        Some(t) => tiers_config
            .get(t)
            .cloned()
            .unwrap_or_else(|| parent_model.to_string()),
        None => parent_model.to_string(),
    }
}

fn parent_tier_name(tiers_config: &TierMap, parent_model: &str) -> String {
    tiers_config
        .iter()
        .find_map(|(tier, model)| {
            if model == parent_model {
                Some(tier.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "balanced".to_string())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryJournalStorage;
    use simulacra_types::{MemoryCapability, MemoryPath, PathPattern};
    use simulacra_vfs::MemoryFs;

    fn parent_with_memory() -> CapabilityToken {
        CapabilityToken {
            paths_read: vec![PathPattern("/**".into())],
            paths_write: vec![PathPattern("/workspace/**".into())],
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
                write_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
            },
            ..Default::default()
        }
    }

    #[test]
    fn override_without_memory_inherits_parent_memory() {
        // W1 regression: when the spawn_agent capabilities override has no
        // memory field, intersecting parent ∩ override must NOT strip the
        // parent's memory grants. The helper inherits parent.memory into
        // the override before intersect.
        let parent = parent_with_memory();
        let override_no_memory = CapabilityToken {
            // Match parent exactly so the path intersection has something to keep —
            // the focus of this test is the memory dimension, not path intersection.
            paths_read: vec![PathPattern("/**".into())],
            ..Default::default()
        };
        let with_memory = inherit_memory_when_override_unset(&override_no_memory, &parent);
        let intersected = parent.intersect(&with_memory);

        assert!(
            intersected.memory.enabled,
            "child must inherit parent memory when override doesn't author memory"
        );
        assert_eq!(
            intersected
                .memory
                .search_scopes
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>(),
            vec!["/var/memory/self"]
        );
    }

    #[test]
    fn override_authoring_memory_is_not_overwritten() {
        // If a future override does author memory (e.g. narrows scopes),
        // the helper must NOT clobber it with parent.memory.
        let parent = parent_with_memory();
        let override_narrower = CapabilityToken {
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self/notes").unwrap()],
                write_scopes: vec![],
            },
            ..Default::default()
        };
        let merged = inherit_memory_when_override_unset(&override_narrower, &parent);
        // Should be the override's value, not parent's.
        assert_eq!(
            merged.memory.search_scopes[0].as_str(),
            "/var/memory/self/notes",
            "helper must not overwrite an override that authored memory"
        );
        assert!(merged.memory.write_scopes.is_empty());
    }

    #[test]
    fn override_with_disabled_default_memory_inherits_parent() {
        // The override carries MemoryCapability::default() (disabled, empty)
        // because parse_capability_override has no JSON path for memory.
        // The helper must inherit parent memory in this case.
        let parent = parent_with_memory();
        let override_default = CapabilityToken::default();
        let merged = inherit_memory_when_override_unset(&override_default, &parent);
        assert!(merged.memory.enabled);
        assert_eq!(merged.memory.search_scopes.len(), 1);
    }

    #[test]
    fn parent_without_memory_means_child_inherits_disabled() {
        // If parent has no memory, the child must also have no memory.
        let parent = CapabilityToken::default();
        let override_default = CapabilityToken::default();
        let merged = inherit_memory_when_override_unset(&override_default, &parent);
        assert!(!merged.memory.enabled);
    }

    #[test]
    fn child_proc_runtime_overlays_child_proc_state_and_delegates_mailbox() {
        let inherited = Arc::new(MemoryFs::new());
        inherited.mkdir("/proc").unwrap();
        inherited.mkdir("/proc/mailbox").unwrap();
        inherited
            .write("/proc/mailbox/report.md", b"report")
            .unwrap();
        let inherited_vfs: Arc<dyn VirtualFs> = inherited;
        let inherited_journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
        let mut capability = CapabilityToken {
            javascript: true,
            ..Default::default()
        };
        capability.paths_read = vec![PathPattern("/**".into())];
        let runtime = child_proc_runtime(
            inherited_vfs,
            inherited_journal,
            ChildProcSpec {
                agent_id: AgentId("child-1".into()),
                agent_name: "researcher".into(),
                model: "child-model".into(),
                parent_id: AgentId("parent-1".into()),
                capability,
                budget: ResourceBudget::new(100, 4, Decimal::ZERO, 0),
                pipeline: None,
            },
        );
        runtime.tools.set(vec![ToolDefinition {
            name: "file_read".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }]);

        assert_eq!(runtime.vfs.read("/proc/agent/id").unwrap(), b"child-1");
        assert_eq!(runtime.vfs.read("/proc/agent/name").unwrap(), b"researcher");
        assert_eq!(
            runtime.vfs.read("/proc/agent/parent_id").unwrap(),
            b"parent-1"
        );
        assert_eq!(
            runtime.vfs.read("/proc/capabilities/javascript").unwrap(),
            b"true"
        );
        assert_eq!(
            runtime.vfs.read("/proc/mailbox/report.md").unwrap(),
            b"report",
            "child-specific ProcFs must still delegate mailbox paths to the inherited stack"
        );
        assert_eq!(
            runtime.vfs.list_dir("/proc/tools").unwrap(),
            vec!["file_read"]
        );
    }
}
