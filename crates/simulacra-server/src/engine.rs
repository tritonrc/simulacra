//! SimulacraEngine — API-triggered agent execution (S034).
//!
//! Bridges the API server to the Simulacra runtime. Constructs fully-configured
//! agents from tenant configuration and spawns them on background tokio tasks,
//! piping activity events back through per-task broadcast channels.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use serde_json::Value;
use simulacra_catalog::repo::{
    AgentRepository, MemoryPoolRepository, SkillRepository, TenantRepository,
};
use simulacra_catalog::{Catalog, CatalogError, CatalogSkillFs, NewAgent, ResolvedAgent, SkillId};
use simulacra_config::SimulacraConfig;
use simulacra_context::ObservationMaskingStrategy;
use simulacra_memory::{Embedder, HitIdCache, MemoryStore, VectorIndex};
use simulacra_provider::{AnthropicProvider, OpenAiProvider};
use simulacra_runtime::{
    ActivitySink, AgentHitlRuntime, AgentLoop, AgentLoopConfig, AgentLoopOutput, AgentSupervisor,
    AgentTaskFactory, CountingJournalStorage, InMemoryJournalStorage, RequestInputTool,
    SpawnAgentTool,
};
use simulacra_sandbox::{AgentCell, ScriptExecutor};
use simulacra_tool::{
    MemoryToolHandles, SkillTool, ToolRegistry, discover_and_filter_skills, register_memory_tools,
};
use simulacra_types::VirtualFs;
use simulacra_types::{ActivityEvent, AgentId, CapabilityToken, ResourceBudget, ToolDefinition};
use simulacra_vfs::{
    HookLister, IntegrationLister, MailboxFs, MemoryFs, MemoryStoreFs, ProcFs, ProcState,
    ReadOnlyPathGuard, ServiceFs, ToolLister,
};
use thiserror::Error;
use tracing::info;

use crate::pool::{AgentWorkerPool, WorkerPoolConfig};
use crate::server::FileAttachment;
use crate::task::{TaskManager, TaskManagerError, TaskState};
use crate::tenant::TenantConfig;

// ──────────────────────────────────────────────────────────────────────────────
// EngineError
// ──────────────────────────────────────────────────────────────────────────────

/// Errors from SimulacraEngine construction and task spawning.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("agent type '{0}' not found in config")]
    AgentTypeNotFound(String),

    #[error("agent '{agent}' not found in catalog for tenant '{tenant}'")]
    AgentNotFound { tenant: String, agent: String },

    #[error("catalog error: {0}")]
    Catalog(String),

    #[error("tenant error: {0}")]
    Tenant(String),

    #[error("provider construction failed: {0}")]
    ProviderError(String),

    #[error("VFS construction failed: {0}")]
    VfsError(String),

    #[error("tool registry construction failed: {0}")]
    ToolRegistryError(String),

    #[error("task manager error: {0}")]
    TaskManager(#[from] TaskManagerError),

    #[error("hook pipeline error: {0}")]
    HookPipelineError(String),

    #[error("missing environment variable: {0}")]
    MissingEnvVar(String),

    #[error("worker pool exhausted — queue at capacity")]
    PoolExhausted,

    #[error("worker pool is shutting down")]
    PoolShutdown,

    #[error("internal error: {0}")]
    Internal(String),
}

impl From<CatalogError> for EngineError {
    fn from(err: CatalogError) -> Self {
        EngineError::Catalog(err.to_string())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Provider resolution
// ──────────────────────────────────────────────────────────────────────────────

/// Provider kind inferred from the model name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    OpenAI,
    Ollama,
}

/// Infer the provider kind from the model string.
///
/// - `"claude-*"` → Anthropic
/// - `"ollama:*"` → Ollama
/// - everything else → OpenAI-compatible
pub fn infer_provider_kind(model: &str) -> Result<ProviderKind, EngineError> {
    if model.starts_with("claude-") {
        Ok(ProviderKind::Anthropic)
    } else if model.starts_with("ollama:") {
        Ok(ProviderKind::Ollama)
    } else {
        Ok(ProviderKind::OpenAI)
    }
}

/// Build an LLM provider from model name.
///
/// Validates that the required API key environment variable is set.
/// Does not actually construct a Provider instance — that requires
/// simulacra-provider types which are wired during full agent construction.
pub fn build_provider(model: &str) -> Result<ProviderKind, EngineError> {
    let kind = infer_provider_kind(model)?;
    match kind {
        ProviderKind::Anthropic => {
            std::env::var("ANTHROPIC_API_KEY")
                .map_err(|_| EngineError::MissingEnvVar("ANTHROPIC_API_KEY".into()))?;
        }
        ProviderKind::OpenAI => {
            std::env::var("OPENAI_API_KEY")
                .map_err(|_| EngineError::MissingEnvVar("OPENAI_API_KEY".into()))?;
        }
        ProviderKind::Ollama => {
            // Ollama doesn't require an API key.
        }
    }
    Ok(kind)
}

// ──────────────────────────────────────────────────────────────────────────────
// ExitReason → TaskState mapping
// ──────────────────────────────────────────────────────────────────────────────

/// Map an `ExitReason` to a `(TaskState, Option<reason>)` pair.
///
/// `AwaitingApproval` maps to `WaitingApproval` (non-terminal).
pub fn map_exit_reason(exit_reason: &simulacra_types::ExitReason) -> (TaskState, Option<String>) {
    use simulacra_types::ExitReason;
    match exit_reason {
        ExitReason::Complete => (TaskState::Completed, None),
        ExitReason::MaxTurns => (TaskState::Completed, Some("max_turns".into())),
        ExitReason::BudgetExhausted => (TaskState::Killed, Some("budget_exhausted".into())),
        ExitReason::Cancelled => (TaskState::Cancelled, None),
        ExitReason::GuardrailTripped(msg) => (TaskState::Killed, Some(format!("guardrail: {msg}"))),
        ExitReason::PolicyKill { hook, reason } => {
            (TaskState::Killed, Some(format!("policy: {hook}: {reason}")))
        }
        ExitReason::Error(msg) => (TaskState::Failed, Some(msg.clone())),
        ExitReason::AwaitingApproval => (TaskState::WaitingApproval, None),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Event translation
// ──────────────────────────────────────────────────────────────────────────────

/// Translate a single (non-ChildActivity) ActivityEvent to server JSON.
fn translate_activity_event(task_id: &str, event: &ActivityEvent) -> Value {
    use serde_json::json;
    match event {
        ActivityEvent::Token { text } => json!({
            "event": "agent.message",
            "task_id": task_id,
            "content": text,
            "role": "assistant",
        }),
        ActivityEvent::ThinkStart => json!({
            "event": "agent.thinking",
            "task_id": task_id,
            "state": "started",
        }),
        ActivityEvent::ThinkDelta { text } => json!({
            "event": "agent.thinking",
            "task_id": task_id,
            "content": text,
        }),
        ActivityEvent::ThinkEnd {
            think_duration_ms,
            think_tokens,
        } => json!({
            "event": "agent.thinking",
            "task_id": task_id,
            "state": "ended",
            "duration_ms": think_duration_ms,
            "tokens": think_tokens,
        }),
        ActivityEvent::ToolStart {
            tool_call_id,
            name,
            arguments,
        } => json!({
            "event": "tool.called",
            "task_id": task_id,
            "tool_call_id": tool_call_id,
            "tool_name": name,
            "arguments": arguments,
        }),
        ActivityEvent::ToolApprovalRequired {
            tool_call_id,
            name,
            arguments,
            reason,
        } => json!({
            "event": "tool.approval_required",
            "task_id": task_id,
            "tool_call_id": tool_call_id,
            "tool_name": name,
            "arguments": arguments,
            "reason": reason,
        }),
        ActivityEvent::ToolCallDelta {
            index,
            tool_call_id,
            name,
            arguments_delta,
        } => json!({
            "event": "tool.call_delta",
            "task_id": task_id,
            "index": index,
            "tool_call_id": tool_call_id,
            "tool_name": name,
            "arguments_delta": arguments_delta,
        }),
        ActivityEvent::ToolOutput { tool_call_id, line } => json!({
            "event": "tool.output",
            "task_id": task_id,
            "tool_call_id": tool_call_id,
            "line": line,
        }),
        ActivityEvent::InputRequired { prompt, schema } => json!({
            "event": "input.required",
            "task_id": task_id,
            "prompt": prompt,
            "schema": schema,
        }),
        ActivityEvent::ToolFinish {
            tool_call_id,
            name,
            is_error,
            duration_ms,
            ..
        } => json!({
            "event": "tool.result",
            "task_id": task_id,
            "tool_call_id": tool_call_id,
            "tool_name": name,
            "is_error": is_error,
            "duration_ms": duration_ms,
        }),
        ActivityEvent::ChildSpawned {
            child_id,
            agent_type,
            task,
        } => json!({
            "event": "agent.child_spawned",
            "task_id": task_id,
            "child_id": child_id,
            "agent_type": agent_type,
            "child_task": task,
        }),
        ActivityEvent::ChildFinished {
            child_id,
            agent_type,
            exit_reason,
            duration_ms,
            ..
        } => json!({
            "event": "agent.child_finished",
            "task_id": task_id,
            "child_id": child_id,
            "agent_type": agent_type,
            "exit_reason": exit_reason,
            "duration_ms": duration_ms,
        }),
        ActivityEvent::TurnComplete => json!({
            "event": "agent.turn_complete",
            "task_id": task_id,
        }),
        ActivityEvent::ChildActivity { .. } => {
            // Should never reach here — ChildActivity is handled by flatten.
            json!({
                "event": "error",
                "task_id": task_id,
                "message": "unexpected ChildActivity in translate_activity_event",
            })
        }
    }
}

/// Recursively flatten `ChildActivity` events, adding child attribution.
///
/// The innermost child_id is preserved for deeply nested events.
fn flatten_activity_event(task_id: &str, event: &ActivityEvent) -> Vec<Value> {
    match event {
        ActivityEvent::ChildActivity {
            child_id,
            agent_type,
            event: inner,
        } => {
            let mut flattened = flatten_activity_event(task_id, inner);
            for evt in &mut flattened {
                // Add child attribution only if not already set (preserves innermost).
                if evt.get("child_id").is_none() {
                    evt["child_id"] = Value::from(child_id.clone());
                    evt["child_agent_type"] = Value::from(agent_type.clone());
                }
            }
            flattened
        }
        other => {
            vec![translate_activity_event(task_id, other)]
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// EngineActivitySink
// ──────────────────────────────────────────────────────────────────────────────

/// Non-blocking activity sink that translates ActivityEvents to server JSON
/// and sends them on the task's event channel.
///
/// Stores a cloned `TaskEventChannel` directly — no TaskManager lock
/// acquisition in the hot path. Every emitted event is appended to the
/// task's history log so that an SSE subscriber connecting after the agent
/// has started still sees the full timeline.
pub struct EngineActivitySink {
    task_id: String,
    sender: crate::task::TaskEventChannel,
    /// Monotonic sequence counter for this task's events.
    seq: AtomicU64,
    /// Optional task manager used to transition HITL activity into waiting states.
    task_manager: Option<TaskManager>,
}

impl EngineActivitySink {
    /// Create a new sink with a cloned task event channel.
    pub fn new(task_id: String, sender: crate::task::TaskEventChannel) -> Self {
        Self {
            task_id,
            sender,
            seq: AtomicU64::new(0),
            task_manager: None,
        }
    }

    /// Create a sink that also mirrors HITL events into TaskManager state.
    pub fn with_task_manager(
        task_id: String,
        sender: crate::task::TaskEventChannel,
        task_manager: TaskManager,
    ) -> Self {
        Self {
            task_id,
            sender,
            seq: AtomicU64::new(0),
            task_manager: Some(task_manager),
        }
    }

    fn transition_for_hitl_event(&self, event: &ActivityEvent) {
        let Some(task_manager) = &self.task_manager else {
            return;
        };
        let result = match event {
            ActivityEvent::InputRequired { .. } => task_manager.request_input(&self.task_id),
            ActivityEvent::ToolApprovalRequired { tool_call_id, .. } => {
                task_manager.request_approval_for(&self.task_id, Some(tool_call_id.clone()))
            }
            _ => return,
        };
        if let Err(err) = result {
            tracing::warn!(
                task_id = %self.task_id,
                error = %err,
                "failed to transition task for HITL activity event"
            );
        }
    }
}

impl simulacra_runtime::ActivitySink for EngineActivitySink {
    fn emit(&self, event: ActivityEvent) {
        // Move HITL tasks into their waiting state before broadcasting the
        // actionable event. Otherwise a fast client can receive the event and
        // send input/approval before TaskManager accepts the response.
        self.transition_for_hitl_event(&event);

        let events = flatten_activity_event(&self.task_id, &event);
        for mut server_event in events {
            let seq = self.seq.fetch_add(1, Ordering::Relaxed);
            server_event["seq"] = Value::from(seq);
            self.sender.send(server_event);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helper types for VFS layers (server mode — no integrations or hooks yet)
// ──────────────────────────────────────────────────────────────────────────────

struct EmptyIntegrationLister;

impl IntegrationLister for EmptyIntegrationLister {
    fn integration_names(&self) -> Vec<String> {
        vec![]
    }
    fn integration_metadata(&self, _name: &str) -> Option<String> {
        None
    }
    fn integration_readme(&self, _name: &str) -> Option<String> {
        None
    }
    fn integration_skill_names(&self, _name: &str) -> Vec<String> {
        vec![]
    }
}

/// Bridges `IntegrationRegistry` into the `IntegrationLister` trait for ServiceFs.
///
/// Adapts the typed registry so that `/svc/<name>/config.json` and
/// `/svc/<name>/README.md` serve live integration metadata without
/// coupling ServiceFs to `simulacra-integration` directly.
struct RegistryIntegrationLister(Arc<simulacra_integration::IntegrationRegistry>);

impl IntegrationLister for RegistryIntegrationLister {
    fn integration_names(&self) -> Vec<String> {
        self.0.names()
    }
    fn integration_metadata(&self, name: &str) -> Option<String> {
        self.0
            .metadata(name)
            .and_then(|m| serde_json::to_string(&m).ok())
    }
    fn integration_readme(&self, name: &str) -> Option<String> {
        let meta = self.0.metadata(name)?;
        let desc = meta
            .description
            .unwrap_or_else(|| format!("{name} integration"));
        Some(format!(
            "# {name}\n\n{desc}\n\n**Base URL:** {}\n**Status:** {}\n",
            meta.base_url, meta.status
        ))
    }
    fn integration_skill_names(&self, _name: &str) -> Vec<String> {
        vec![]
    }
}

struct EmptyHookLister;

impl HookLister for EmptyHookLister {
    fn hook_names(&self, _operation: &str) -> Vec<String> {
        vec![]
    }
}

#[derive(Clone, Default)]
struct SharedToolList(Arc<Mutex<Vec<ToolDefinition>>>);

impl SharedToolList {
    fn set(&self, defs: Vec<ToolDefinition>) {
        *self.0.lock().unwrap() = defs;
    }
}

impl ToolLister for SharedToolList {
    fn tool_names(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.name.clone())
            .collect()
    }
    fn tool_json(&self, name: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.name == name)
            .and_then(|d| serde_json::to_string(d).ok())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// SimulacraEngine
// ──────────────────────────────────────────────────────────────────────────────

/// Per-task snapshot frozen at spawn time. Held alongside the workspace VFS
/// so the resolved agent the worker actually consumed remains observable
/// even after the catalog row mutates.
#[derive(Clone)]
struct ResolvedAgentSnapshot {
    resolved: ResolvedAgent,
    composed_vfs: Arc<dyn VirtualFs>,
    capability_token: CapabilityToken,
}

/// The composition root bridging the API server to the Simulacra runtime.
///
/// Owns the `SimulacraConfig` and optional `IntegrationRegistry`, and constructs
/// fully-isolated agent environments on each `spawn_task` call.
pub struct SimulacraEngine {
    config: SimulacraConfig,
    integration_registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
    pool: Arc<AgentWorkerPool>,
    /// Durable artifact storage for `/proc/mailbox/` writes.
    artifact_store: Arc<dyn simulacra_types::ArtifactStore>,
    /// Per-task workspace VFS snapshots for testing/debugging.
    workspace_snapshots: Mutex<HashMap<String, Arc<MemoryFs>>>,
    /// Per-task ResolvedAgent + composed-VFS + capability snapshots, frozen
    /// at spawn time so post-spawn catalog mutations cannot leak into a
    /// running task (spec S042 assertion 6).
    resolved_snapshots: Mutex<HashMap<String, ResolvedAgentSnapshot>>,
    // ── Catalog repositories (S042) ──
    //
    // `agents` and `tenants` are read in `spawn_task` to resolve the catalog
    // row that backs each task. `skills` and `memory_pools` are accepted
    // here for symmetry with the S042 trait surface even though
    // `AgentRepository::resolve` already returns the joined skills + pool —
    // holding them on the engine keeps the public ctor signature aligned
    // with the four-repo catalog seam without forcing every test/example
    // call site to plumb conditionally. They become directly read in a
    // future spec when individual create/update mutations grow runtime
    // surfaces (e.g. live skill reload during a paused task).
    agents: Arc<dyn AgentRepository>,
    #[allow(dead_code)]
    skills: Arc<dyn SkillRepository>,
    #[allow(dead_code)]
    memory_pools: Arc<dyn MemoryPoolRepository>,
    tenants: Arc<dyn TenantRepository>,
    // ── Memory (optional; S037) ──
    memory_store: Option<Arc<dyn MemoryStore>>,
    vector_index: Option<Arc<dyn VectorIndex>>,
    embedder: Option<Arc<dyn Embedder>>,
    /// Process-wide hit id cache. Always present so the tool layer can mint
    /// ids without branching on memory availability.
    hit_cache: Arc<HitIdCache>,
    /// Governance hook pipeline threaded into tool_call interception.
    /// Always present (possibly empty) so memory tools receive `Some(...)`
    /// per S037 §20 "wired... not `None`". External callers may replace
    /// this before constructing the engine to register hooks.
    hook_pipeline: Arc<simulacra_hooks::HookPipeline>,
    /// S043 — Optional test-only LLM provider factory. When `Some`,
    /// `spawn_task` consults this closure instead of constructing
    /// production `AnthropicProvider`/`OpenAiProvider` from env vars.
    /// `None` in every production code path; installed only by tests
    /// via [`SimulacraEngine::with_provider_factory`].
    provider_factory: Option<ProviderFactory>,
    /// S045 — Byte storage for per-agent files. Wired by callers via
    /// [`SimulacraEngine::with_agent_file_store`]. When `None`, agents that
    /// have files will spawn but `/var/agent_files/` will be empty and a
    /// warning is logged. We do NOT fail spawn — the catalog metadata can
    /// outlive a transient store misconfig, and the agent shouldn't be
    /// blocked from running on data it can fetch via REST.
    agent_file_store: Option<Arc<dyn simulacra_catalog::AgentFileStore>>,
}

/// S043 — Test-only provider factory closure. The factory receives the
/// resolved [`ProviderKind`] and the agent's `model` string and returns a
/// `Box<dyn Provider>` that the agent loop will drive.
///
/// Kept as a type alias rather than a public trait to keep the surface
/// minimal: this is a test-injection point, not a public extension API.
/// Production code never installs a factory; tests install one via
/// [`SimulacraEngine::with_provider_factory`].
pub type ProviderFactory = Arc<
    dyn Fn(ProviderKind, &str) -> Result<Box<dyn simulacra_types::Provider>, EngineError>
        + Send
        + Sync,
>;

impl std::fmt::Debug for SimulacraEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimulacraEngine")
            .field("config", &self.config.project.name)
            .finish()
    }
}

impl SimulacraEngine {
    /// Construct the engine with explicit catalog repositories.
    pub fn new(
        config: SimulacraConfig,
        integration_registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
        agents: Arc<dyn AgentRepository>,
        skills: Arc<dyn SkillRepository>,
        memory_pools: Arc<dyn MemoryPoolRepository>,
        tenants: Arc<dyn TenantRepository>,
    ) -> Result<Self, EngineError> {
        Self::with_pool_config(
            config,
            integration_registry,
            WorkerPoolConfig::default(),
            agents,
            skills,
            memory_pools,
            tenants,
        )
    }

    /// Construct the engine with explicit pool configuration.
    /// Uses the default artifact store at `/tmp/simulacra-artifacts`.
    #[allow(clippy::too_many_arguments)]
    pub fn with_pool_config(
        config: SimulacraConfig,
        integration_registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
        pool_config: WorkerPoolConfig,
        agents: Arc<dyn AgentRepository>,
        skills: Arc<dyn SkillRepository>,
        memory_pools: Arc<dyn MemoryPoolRepository>,
        tenants: Arc<dyn TenantRepository>,
    ) -> Result<Self, EngineError> {
        let artifact_store: Arc<dyn simulacra_types::ArtifactStore> = Arc::new(
            crate::LocalDiskArtifactStore::new(std::path::Path::new("/tmp/simulacra-artifacts"))
                .map_err(|e| EngineError::VfsError(format!("artifact store: {e}")))?,
        );
        Self::with_components(
            config,
            integration_registry,
            pool_config,
            artifact_store,
            agents,
            skills,
            memory_pools,
            tenants,
        )
    }

    /// Construct the engine with explicit components — the most flexible constructor.
    /// Prefer this in tests or when the artifact store location matters.
    ///
    /// The engine's artifact store is the single source of truth for all task artifacts.
    /// Always use `AppState::with_engine(engine)` so HTTP routes read from the same store
    /// the engine writes to.
    #[allow(clippy::too_many_arguments)]
    pub fn with_components(
        config: SimulacraConfig,
        integration_registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
        pool_config: WorkerPoolConfig,
        artifact_store: Arc<dyn simulacra_types::ArtifactStore>,
        agents: Arc<dyn AgentRepository>,
        skills: Arc<dyn SkillRepository>,
        memory_pools: Arc<dyn MemoryPoolRepository>,
        tenants: Arc<dyn TenantRepository>,
    ) -> Result<Self, EngineError> {
        let pool = Arc::new(AgentWorkerPool::new(pool_config));
        Ok(Self {
            config,
            integration_registry,
            pool,
            artifact_store,
            workspace_snapshots: Mutex::new(HashMap::new()),
            resolved_snapshots: Mutex::new(HashMap::new()),
            agents,
            skills,
            memory_pools,
            tenants,
            memory_store: None,
            vector_index: None,
            embedder: None,
            hit_cache: Arc::new(HitIdCache::new()),
            hook_pipeline: Arc::new(simulacra_hooks::HookPipeline::new()),
            provider_factory: None,
            agent_file_store: None,
        })
    }

    /// Construct the engine with memory wired up. Memory-enabled agent types
    /// will receive `semantic_search` / `memory_read_chunk` tools at
    /// registration time; agent types without `memory.enabled = true` will
    /// not see the tools even when memory is available on the engine.
    #[allow(clippy::too_many_arguments)]
    pub fn with_memory(
        config: SimulacraConfig,
        integration_registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
        artifact_store: Arc<dyn simulacra_types::ArtifactStore>,
        memory_store: Arc<dyn MemoryStore>,
        vector_index: Arc<dyn VectorIndex>,
        embedder: Arc<dyn Embedder>,
        agents: Arc<dyn AgentRepository>,
        skills: Arc<dyn SkillRepository>,
        memory_pools: Arc<dyn MemoryPoolRepository>,
        tenants: Arc<dyn TenantRepository>,
    ) -> Result<Self, EngineError> {
        let pool = Arc::new(AgentWorkerPool::new(WorkerPoolConfig::default()));
        Ok(Self {
            config,
            integration_registry,
            pool,
            artifact_store,
            workspace_snapshots: Mutex::new(HashMap::new()),
            resolved_snapshots: Mutex::new(HashMap::new()),
            agents,
            skills,
            memory_pools,
            tenants,
            memory_store: Some(memory_store),
            vector_index: Some(vector_index),
            embedder: Some(embedder),
            hit_cache: Arc::new(HitIdCache::new()),
            hook_pipeline: Arc::new(simulacra_hooks::HookPipeline::new()),
            provider_factory: None,
            agent_file_store: None,
        })
    }

    /// Construct an engine backed by a fresh in-memory catalog seeded from
    /// `config.agent_types`, `config.tenants`, and `config.memory`. Used by
    /// tests and by `--no-catalog` CLI mode.
    pub async fn new_with_in_memory_catalog(
        config: SimulacraConfig,
        integration_registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
    ) -> Result<Self, EngineError> {
        let (agents, skills, memory_pools, tenants) = build_in_memory_catalog(&config).await?;
        Self::new(
            config,
            integration_registry,
            agents,
            skills,
            memory_pools,
            tenants,
        )
    }

    /// S043 — Install a test-only provider factory. Returns `self` so that
    /// callers can chain: `SimulacraEngine::new(...)?.with_provider_factory(f)`.
    ///
    /// When set, `spawn_task` invokes this factory in place of the production
    /// `AnthropicProvider`/`OpenAiProvider` construction site. The factory
    /// receives the resolved [`ProviderKind`] and the agent's `model` string;
    /// it returns the `Box<dyn Provider>` the agent loop will drive.
    ///
    /// Installing a factory bypasses env-var validation (`build_provider`
    /// still infers the kind, but the production env-var check is skipped
    /// when an override is present — overriding implies the caller knows
    /// what they're substituting).
    pub fn with_provider_factory(mut self, factory: ProviderFactory) -> Self {
        self.provider_factory = Some(factory);
        self
    }

    /// S045 — Wire the byte storage backend for per-agent files. Without
    /// this, agents that have files spawn but `/var/agent_files/` is
    /// empty and a warning is logged. Tests + production wire the same
    /// store the catalog uses (`Catalog::agent_file_store()`).
    pub fn with_agent_file_store(
        mut self,
        store: Arc<dyn simulacra_catalog::AgentFileStore>,
    ) -> Self {
        self.agent_file_store = Some(store);
        self
    }

    /// S043 — Test accessor. Returns `true` iff a provider factory has been
    /// installed via [`SimulacraEngine::with_provider_factory`]. Used by tests
    /// to fail fast if the override was forgotten.
    pub fn debug_provider_factory_is_set(&self) -> bool {
        self.provider_factory.is_some()
    }

    /// Async variant of [`Self::with_pool_config`] using a seeded in-memory catalog.
    pub async fn with_pool_config_in_memory_catalog(
        config: SimulacraConfig,
        integration_registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
        pool_config: WorkerPoolConfig,
    ) -> Result<Self, EngineError> {
        let (agents, skills, memory_pools, tenants) = build_in_memory_catalog(&config).await?;
        Self::with_pool_config(
            config,
            integration_registry,
            pool_config,
            agents,
            skills,
            memory_pools,
            tenants,
        )
    }

    /// Async variant of [`Self::with_components`] using a seeded in-memory catalog.
    pub async fn with_components_in_memory_catalog(
        config: SimulacraConfig,
        integration_registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
        pool_config: WorkerPoolConfig,
        artifact_store: Arc<dyn simulacra_types::ArtifactStore>,
    ) -> Result<Self, EngineError> {
        let (agents, skills, memory_pools, tenants) = build_in_memory_catalog(&config).await?;
        Self::with_components(
            config,
            integration_registry,
            pool_config,
            artifact_store,
            agents,
            skills,
            memory_pools,
            tenants,
        )
    }

    /// Async variant of [`Self::with_memory`] using a seeded in-memory catalog.
    #[allow(clippy::too_many_arguments)]
    pub async fn with_memory_in_memory_catalog(
        config: SimulacraConfig,
        integration_registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
        artifact_store: Arc<dyn simulacra_types::ArtifactStore>,
        memory_store: Arc<dyn MemoryStore>,
        vector_index: Arc<dyn VectorIndex>,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Self, EngineError> {
        let (agents, skills, memory_pools, tenants) = build_in_memory_catalog(&config).await?;
        Self::with_memory(
            config,
            integration_registry,
            artifact_store,
            memory_store,
            vector_index,
            embedder,
            agents,
            skills,
            memory_pools,
            tenants,
        )
    }

    /// Replace the engine's hook pipeline. Must be called before tasks are
    /// spawned so pre-registered hooks are visible to the memory tools.
    pub fn set_hook_pipeline(&mut self, pipeline: Arc<simulacra_hooks::HookPipeline>) {
        self.hook_pipeline = pipeline;
    }

    /// Return a clone of the engine's hook pipeline Arc.
    pub fn hook_pipeline(&self) -> &Arc<simulacra_hooks::HookPipeline> {
        &self.hook_pipeline
    }

    /// Return the shared hit id cache. Callers constructing an `AppState`
    /// with memory enabled can reuse this instead of creating a new one so
    /// the server and engine agree on minted hit ids.
    pub fn hit_cache(&self) -> &Arc<HitIdCache> {
        &self.hit_cache
    }

    /// Return the memory store if configured.
    pub fn memory_store(&self) -> Option<&Arc<dyn MemoryStore>> {
        self.memory_store.as_ref()
    }

    /// Return the vector index if configured.
    pub fn vector_index(&self) -> Option<&Arc<dyn VectorIndex>> {
        self.vector_index.as_ref()
    }

    /// Return the embedder if configured.
    pub fn embedder(&self) -> Option<&Arc<dyn Embedder>> {
        self.embedder.as_ref()
    }

    /// Returns a reference to the worker pool.
    pub fn pool(&self) -> &Arc<AgentWorkerPool> {
        &self.pool
    }

    /// S045 — REST handlers need a way to translate `TenantConfig.namespace`
    /// (the auth/resolver layer's identifier) into the catalog's
    /// `TenantId`. Expose the tenants repo so the upload/download routes
    /// can do a single namespace→id lookup before touching the file repo.
    pub fn tenants_repo(&self) -> Arc<dyn TenantRepository> {
        Arc::clone(&self.tenants)
    }

    /// Returns a reference to the artifact store. `AppState` should reuse this
    /// so HTTP artifact routes read from the same backend that agents write to.
    pub fn artifact_store(&self) -> &Arc<dyn simulacra_types::ArtifactStore> {
        &self.artifact_store
    }

    /// Evict the workspace snapshot and resolved-agent snapshot for a
    /// completed task to free memory.
    pub fn evict_workspace_snapshot(&self, task_id: &str) {
        if let Ok(mut snapshots) = self.workspace_snapshots.lock() {
            snapshots.remove(task_id);
        }
        if let Ok(mut snapshots) = self.resolved_snapshots.lock() {
            snapshots.remove(task_id);
        }
    }

    /// Return the per-task `ResolvedAgent` captured at spawn time.
    ///
    /// The snapshot is frozen at spawn time and not affected by subsequent
    /// catalog mutations.
    pub fn debug_resolved_agent(&self, task_id: &str) -> Option<ResolvedAgent> {
        let snapshots = self.resolved_snapshots.lock().ok()?;
        snapshots.get(task_id).map(|s| s.resolved.clone())
    }

    /// Return the per-task composed VFS stack with catalog skills snapshotted
    /// at `/skills/<name>/SKILL.md` and `/var/skills/<name>.md`.
    pub fn debug_composed_vfs(&self, task_id: &str) -> Option<Arc<dyn VirtualFs>> {
        let snapshots = self.resolved_snapshots.lock().ok()?;
        snapshots.get(task_id).map(|s| Arc::clone(&s.composed_vfs))
    }

    /// Return the per-task `CapabilityToken` built from the resolved agent.
    pub fn debug_capability_token(&self, task_id: &str) -> Option<CapabilityToken> {
        let snapshots = self.resolved_snapshots.lock().ok()?;
        snapshots.get(task_id).map(|s| s.capability_token.clone())
    }

    /// Create a task, construct the agent, spawn it, and return the task handle.
    ///
    /// This method owns the full lifecycle:
    /// 1. Resolves the agent from the catalog
    /// 2. Calls TaskManager::create_task
    /// 3. Extracts broadcast sender
    /// 4. Validates provider (env vars)
    /// 5. Spawns background agent task
    /// 6. Returns TaskHandle immediately
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_task(
        &self,
        task_manager: &TaskManager,
        description: &str,
        tenant: &TenantConfig,
        agent_type_override: Option<&str>,
        metadata: Value,
        files: Option<HashMap<String, FileAttachment>>,
        connection_id: Option<String>,
    ) -> Result<crate::task::TaskHandle, EngineError> {
        let agent_type_name = agent_type_override
            .map(str::to_owned)
            .unwrap_or_else(|| tenant.agent_type.clone());
        let enable_human_input = metadata
            .get("enable_human_input")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let require_tool_approval = metadata
            .get("require_tool_approval")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let hitl_enabled = enable_human_input || require_tool_approval;

        // Resolve tenant from catalog.
        let tenant_row = self
            .tenants
            .get_by_namespace(&tenant.namespace)
            .await
            .map_err(|e| match e {
                CatalogError::NotFound(_) => EngineError::Tenant(format!(
                    "tenant namespace not in catalog: {}",
                    tenant.namespace
                )),
                other => EngineError::Catalog(other.to_string()),
            })?;

        // Resolve agent from catalog.
        // S042 o11y: `simulacra.engine.resolve_agent` span carries tenant_id +
        // agent_name on entry, and (on success) agent_id. Failure paths
        // record the structured error before returning. Full o11y suite
        // (counter, histogram, and per-graphql/per-catalog spans) is tracked
        // in a follow-up spec; this minimal span anchors the core seam.
        let resolve_span = tracing::info_span!(
            "simulacra.engine.resolve_agent",
            tenant_id = %tenant_row.id.as_str(),
            tenant_namespace = %tenant.namespace,
            agent_name = %agent_type_name,
            agent_id = tracing::field::Empty,
        );
        let resolved: ResolvedAgent = {
            let _enter = resolve_span.enter();
            match self.agents.resolve(&tenant_row.id, &agent_type_name).await {
                Ok(r) => {
                    resolve_span.record("agent_id", r.id.as_str());
                    r
                }
                Err(CatalogError::NotFound(_)) => {
                    return Err(EngineError::AgentNotFound {
                        tenant: tenant.namespace.clone(),
                        agent: agent_type_name.clone(),
                    });
                }
                Err(other) => return Err(EngineError::Catalog(other.to_string())),
            }
        };

        let model = resolved.model.clone();

        // Create task in TaskManager in Pending state (pool model).
        let handle = task_manager.create_pending_task(
            tenant,
            description,
            Some(agent_type_name.clone()),
            metadata,
            connection_id,
        )?;

        let task_id = handle.task_id.clone();

        // Extract broadcast sender for event bridging.
        let sender = match task_manager.get_event_sender(&task_id) {
            Ok(s) => s,
            Err(e) => {
                let _ = task_manager.complete_task(
                    &task_id,
                    TaskState::Failed,
                    Some(format!("failed to get event sender: {e}")),
                );
                return Err(EngineError::Internal(format!(
                    "failed to get event sender: {e}"
                )));
            }
        };

        // Validate provider. Production path checks env vars via
        // `build_provider`; with an S043 provider override installed,
        // skip env-var validation (the override is what will run, env
        // vars are irrelevant to the substituted provider).
        if self.provider_factory.is_none()
            && let Err(e) = build_provider(&model)
        {
            let _ = task_manager.complete_task(&task_id, TaskState::Failed, Some(e.to_string()));
            return Err(e);
        }

        let sink = Arc::new(EngineActivitySink::with_task_manager(
            task_id.clone(),
            sender,
            task_manager.clone(),
        ));

        let cancel_token = tokio_util::sync::CancellationToken::new();
        let _ = task_manager.set_cancellation_token(&task_id, cancel_token.clone());

        let hitl_runtime = if hitl_enabled {
            let (senders, runtime) = AgentHitlRuntime::channel_pair(require_tool_approval);
            task_manager.set_hitl_senders(&task_id, senders.input_tx, senders.approval_tx)?;
            Some(runtime)
        } else {
            None
        };

        let system_prompt = resolved.system_prompt.clone();

        // Budget from resolved + tenant pool fallbacks.
        let max_tokens =
            resolved
                .max_tokens
                .map(|n| n as u64)
                .unwrap_or(if tenant.budget_pool.max_tokens > 0 {
                    tenant.budget_pool.max_tokens
                } else {
                    0
                });
        let max_turns = resolved.max_turns;
        let max_cost = if tenant.budget_pool.max_cost.is_empty() {
            Decimal::ZERO
        } else {
            tenant
                .budget_pool
                .max_cost
                .parse::<Decimal>()
                .unwrap_or(Decimal::ZERO)
        };
        let max_sub_agents: u32 = 0;
        let budget = ResourceBudget::new(max_tokens, max_turns, max_cost, max_sub_agents);

        // Build capability token from the resolved agent's catalog capabilities.
        let capability_token = build_capability_token_from_resolved(&resolved);

        // Step 10: Spawn background task with real agent construction.
        let task_manager_clone = task_manager.clone();
        let description_owned = description.to_string();
        let agent_type_name_owned = agent_type_name.to_string();
        let integration_registry = self.integration_registry.clone();
        let config_for_worker = self.config.clone();

        info!(
            task_id = %task_id,
            tenant = %tenant.namespace,
            agent_type = %agent_type_name,
            model = %model,
            "spawning agent task"
        );

        let provider_kind = infer_provider_kind(&model)?;
        let runtime_provider_kind = runtime_provider_kind(provider_kind);
        let model_clone = model.clone();

        // Determine tenant integrations: use tenant config if available,
        // fall back to all integrations in single-tenant (CLI) mode.
        let tenant_integrations: Vec<String> = if !tenant.integrations.is_empty() {
            tenant.integrations.clone()
        } else if self.config.tenants.is_empty() {
            // Single-tenant / CLI mode — grant all integrations.
            self.integration_registry
                .as_ref()
                .map(|r| r.names())
                .unwrap_or_default()
        } else {
            // Multi-tenant mode with empty integrations list — no grants.
            vec![]
        };
        let tenant_mcp_servers: Vec<String> = if !tenant.mcp_servers.is_empty() {
            tenant.mcp_servers.clone()
        } else if self.config.tenants.is_empty() {
            self.config
                .mcp
                .as_ref()
                .map(|mcp| {
                    mcp.servers
                        .iter()
                        .map(|server| server.name.clone())
                        .collect()
                })
                .unwrap_or_default()
        } else {
            vec![]
        };

        // Pre-build the MemoryFs and seed files so we can store a snapshot reference.
        let memory_fs = Arc::new(MemoryFs::new());
        let _ = memory_fs.mkdir("/workspace");
        memory_fs
            .write("/workspace/task.md", description_owned.as_bytes())
            .map_err(|e| EngineError::VfsError(format!("failed to seed task.md: {e}")))?;

        // Seed file attachments into the workspace.
        if let Some(ref file_map) = files {
            for (filename, attachment) in file_map {
                let bytes = match attachment.encoding.as_deref() {
                    Some("base64") => {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD
                            .decode(&attachment.data)
                            .map_err(|e| {
                                EngineError::VfsError(format!(
                                    "invalid base64 in '{filename}': {e}"
                                ))
                            })?
                    }
                    _ => attachment.data.as_bytes().to_vec(),
                };
                let path = format!("/workspace/{filename}");
                // Create parent directories if needed (e.g. "reports/q1.csv").
                if let Some(parent) = path.rsplit_once('/').map(|(p, _)| p.to_string())
                    && parent != "/workspace"
                {
                    let _ = memory_fs.mkdir(&parent);
                }
                memory_fs.write(&path, &bytes).map_err(|e| {
                    EngineError::VfsError(format!("failed to seed '{filename}': {e}"))
                })?;
            }
        }

        // Mount catalog skills at canonical /skills/<name>/SKILL.md paths for
        // S017 discovery, and preserve /var/skills/<name>.md as a compatibility
        // debug path. Bodies are pre-rendered via CatalogSkillFs so YAML
        // frontmatter handling stays consistent with the read-only fs view.
        let _ = memory_fs.mkdir("/skills");
        let _ = memory_fs.mkdir("/var");
        let _ = memory_fs.mkdir("/var/skills");
        let skill_fs = CatalogSkillFs::new(resolved.skills.clone());
        for skill in &resolved.skills {
            validate_catalog_skill_name_for_vfs(&skill.name)?;
            let rendered_path = format!("/{}/SKILL.md", skill.name);
            let body = skill_fs.read(&rendered_path).map_err(|e| {
                EngineError::VfsError(format!(
                    "failed to render catalog skill '{}': {e}",
                    skill.name
                ))
            })?;
            let canonical_path = format!("/skills/{}/SKILL.md", skill.name);
            memory_fs.write(&canonical_path, &body).map_err(|e| {
                EngineError::VfsError(format!("failed to mount catalog skill: {e}"))
            })?;
            let mount_path = format!("/var/skills/{}.md", skill.name);
            memory_fs.write(&mount_path, &body).map_err(|e| {
                EngineError::VfsError(format!("failed to mount catalog skill: {e}"))
            })?;
        }

        // S045 — Mount per-agent files at /var/agent_files/<name>. Bytes
        // are pre-loaded from the AgentFileStore at spawn so that detach
        // after spawn does not strip bytes from the running task — the
        // running task holds its own snapshot in memory_fs.
        let _ = memory_fs.mkdir("/var/agent_files");
        if !resolved.files.is_empty() {
            match self.agent_file_store.as_ref() {
                Some(store) => {
                    for file in &resolved.files {
                        let bytes = store.get(&file.id).await.map_err(|e| {
                            EngineError::VfsError(format!(
                                "failed to load agent file '{}': {e}",
                                file.name
                            ))
                        })?;
                        let mount_path = format!("/var/agent_files/{}", file.name);
                        memory_fs.write(&mount_path, &bytes).map_err(|e| {
                            EngineError::VfsError(format!(
                                "failed to mount agent file '{}': {e}",
                                file.name
                            ))
                        })?;
                    }
                }
                None => {
                    tracing::warn!(
                        agent = %resolved.name,
                        files = resolved.files.len(),
                        "agent has files but no AgentFileStore is wired; /var/agent_files/ will be empty",
                    );
                }
            }
        }

        // Store snapshot for debug_workspace_snapshot.
        {
            let mut snapshots = self.workspace_snapshots.lock().unwrap();
            snapshots.insert(task_id.clone(), Arc::clone(&memory_fs));
        }

        // Freeze the resolved agent + composed VFS + capability token for
        // later inspection. Catalog mutations after this point cannot reach
        // the running task (S042 assertion 6).
        //
        // S042/S045 — these mounts hold snapshot copies in memory_fs (writable),
        // but the composed/runtime VFS must reject write/mkdir/remove attempts
        // under their read-only namespaces.
        let runtime_root_vfs =
            read_only_spawn_snapshot_paths(Arc::clone(&memory_fs) as Arc<dyn VirtualFs>);
        let composed_vfs = Arc::clone(&runtime_root_vfs);
        {
            let mut snapshots = self.resolved_snapshots.lock().unwrap();
            snapshots.insert(
                task_id.clone(),
                ResolvedAgentSnapshot {
                    resolved: resolved.clone(),
                    composed_vfs: Arc::clone(&composed_vfs),
                    capability_token: capability_token.clone(),
                },
            );
        }

        // Submit work item to the worker pool.
        // Each work item builds a fresh current_thread runtime (per-work-item, not per-worker).
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let task_manager_for_worker = task_manager.clone();
        let task_id_for_worker = task_id.clone();
        let artifact_store = Arc::clone(&self.artifact_store);
        let tenant_namespace = tenant.namespace.clone();
        // Memory handles (cloned into the worker closure). When any of the
        // three is `None`, memory tool registration is skipped inside the
        // closure regardless of the agent's capability.
        let memory_store_opt = self.memory_store.as_ref().map(Arc::clone);
        let vector_index_opt = self.vector_index.as_ref().map(Arc::clone);
        let embedder_opt = self.embedder.as_ref().map(Arc::clone);
        let hit_cache = Arc::clone(&self.hit_cache);
        let hook_pipeline = Arc::clone(&self.hook_pipeline);
        let hitl_runtime_for_worker = hitl_runtime.clone();
        let agent_skill_names: Vec<String> = resolved
            .skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect();
        // S043 — clone the optional provider factory into the worker closure.
        // `None` in production; `Some(...)` only when a test installed an
        // override via `with_provider_factory`.
        let provider_factory_for_worker = self.provider_factory.clone();

        self.pool.submit(Box::new(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = result_tx.send(Err(format!("failed to build agent runtime: {e}")));
                    return;
                }
            };
            let result = rt.block_on(async move {
                // Transition Pending -> Running now that a worker has picked us up.
                if let Err(e) = task_manager_for_worker.start_task(&task_id_for_worker) {
                    return Err(format!("failed to start task: {e}"));
                }
                // 1. Build VFS stack (bottom-up):
                //   MemoryFs (workspace)
                //     → MailboxFs (/proc/mailbox/** → ArtifactStore)
                //       → MemoryStoreFs (/var/memory/**, /mnt/** → MemoryStore) [conditional]
                //         → ServiceFs (/svc/**)
                //           → ProcFs (/proc/**)
                //
                // MemoryStoreFs is only installed when the agent's
                // MemoryCapability is enabled AND the engine has memory
                // handles. When disabled, memory paths fall through to the
                // inner VFS (MemoryFs) and return NotFound — enforcing the
                // opt-in model per S037 §14.
                let inner_vfs: Arc<dyn simulacra_types::VirtualFs> = runtime_root_vfs;

                // 1b. Wrap with MailboxFs (persists /proc/mailbox/** to artifact store).
                //
                // The sink emits an `artifact.created` SSE event on the same
                // task event channel as `EngineActivitySink` — TaskManager's
                // `emit_event` manages the seq counter and history log, so
                // the event is properly ordered for SSE replay (see
                // task.rs::TaskManagerInner::emit_event). The agent-run UI
                // listens for this event to refresh its artifacts sidebar
                // (see assets/components/agent-run.js).
                let mailbox_task_id = task_id.clone();
                let mailbox_task_manager = task_manager_for_worker.clone();
                let artifact_sink: simulacra_vfs::ArtifactWriteSink =
                    Arc::new(move |path: &str, _tenant: &str, size: u64| {
                        if let Err(e) = mailbox_task_manager.emit_event(
                            &mailbox_task_id,
                            serde_json::json!({
                                "event": "artifact.created",
                                "task_id": mailbox_task_id,
                                "path": path,
                                "size": size,
                            }),
                        ) {
                            tracing::warn!(
                                task_id = %mailbox_task_id,
                                error = %e,
                                "failed to emit artifact.created event"
                            );
                        }
                    });
                let with_mailbox: Arc<dyn simulacra_types::VirtualFs> = Arc::new(
                    MailboxFs::new(
                        inner_vfs,
                        task_id.clone(),
                        tenant_namespace.clone(),
                        Arc::clone(&artifact_store),
                    )
                    .with_artifact_sink(artifact_sink),
                );

                // 1c. Create the per-run RecentWritesBuffer (RRWB). This is
                // the same Arc we pass to MemoryStoreFs (for write-time
                // recording) and to the memory tools (for search-time
                // merge) — see S037 §7 Guarantee 2.
                let rrwb: Arc<std::sync::Mutex<simulacra_memory::RecentWritesBuffer>> =
                    Arc::new(std::sync::Mutex::new(
                        simulacra_memory::RecentWritesBuffer::new(),
                    ));

                // 1d. Conditionally wrap with MemoryStoreFs.
                let with_memory: Arc<dyn simulacra_types::VirtualFs> = if capability_token
                    .memory
                    .enabled
                    && let Some(mem_store) = memory_store_opt.as_ref()
                {
                    match simulacra_types::TenantId::parse(&tenant_namespace) {
                        Ok(tenant_id) => Arc::new(
                            MemoryStoreFs::new(
                                with_mailbox,
                                tenant_id,
                                Arc::clone(mem_store),
                                capability_token.memory.clone(),
                            )
                            .with_rrwb(Arc::clone(&rrwb)),
                        ),
                        Err(e) => {
                            tracing::warn!(
                                tenant = %tenant_namespace,
                                error = %e,
                                "memory enabled but tenant namespace is not a valid TenantId; skipping MemoryStoreFs install"
                            );
                            with_mailbox
                        }
                    }
                } else {
                    with_mailbox
                };

                // 2. Wrap with ServiceFs
                let integration_lister: Arc<dyn IntegrationLister> = match &integration_registry {
                    Some(reg) => Arc::new(RegistryIntegrationLister(Arc::clone(reg))),
                    None => Arc::new(EmptyIntegrationLister),
                };
                let with_svc = ServiceFs::new(with_memory, integration_lister);

                // 3. Wrap with ProcFs
                let budget_arc = Arc::new(Mutex::new(budget));
                let proc_turn = Arc::new(AtomicU64::new(0));
                let proc_journal_entries = Arc::new(AtomicU64::new(0));
                let shared_tools = SharedToolList::default();
                let proc_state = Arc::new(ProcState {
                    agent_id: task_id.clone(),
                    agent_name: agent_type_name_owned.clone(),
                    model: model_clone.clone(),
                    parent_id: None,
                    budget: Arc::clone(&budget_arc),
                    capabilities: capability_token.clone(),
                    tools: Arc::new(shared_tools.clone()),
                    session_id: task_id.clone(),
                    session_start: std::time::Instant::now(),
                    journal_entries: Arc::clone(&proc_journal_entries),
                    hooks: Arc::new(EmptyHookLister),
                    turn: Arc::clone(&proc_turn),
                });
                let vfs: Arc<dyn simulacra_types::VirtualFs> =
                    Arc::new(ProcFs::new(with_svc, proc_state));

                // 4. Build journal (in-memory for server mode)
                let inner_journal: Arc<dyn simulacra_types::JournalStorage> =
                    Arc::new(InMemoryJournalStorage::new());
                let journal: Arc<dyn simulacra_types::JournalStorage> = Arc::new(
                    CountingJournalStorage::new(inner_journal, Arc::clone(&proc_journal_entries)),
                );

                // 5. Build HTTP client
                let http_client: Arc<dyn simulacra_http::HttpClient> =
                    Arc::new(simulacra_http::UreqHttpClient::new(30_000, 10));

                // 6. Build AgentCell
                let mut cell = AgentCell::new(
                    Arc::clone(&vfs),
                    capability_token.clone(),
                    Arc::clone(&budget_arc),
                    Arc::clone(&journal),
                    http_client,
                );
                cell.set_script_executor(ScriptExecutor::new(4));
                // Wire integration registry for credential injection into fetch()
                if let Some(ref reg) = integration_registry {
                    cell.integration_registry = Some(Arc::clone(reg));
                    // Use tenant-scoped integrations (not reg.names()).
                    cell.tenant_integrations = tenant_integrations.clone();
                }
                let cell = Arc::new(cell);

                // 7. Build ToolRegistry
                let mut registry = ToolRegistry::new();
                simulacra_tool::register_builtins(&mut registry, Arc::clone(&cell))
                    .map_err(|e| format!("failed to register built-in tools: {e}"))?;

                if enable_human_input
                    && let Some(hitl_runtime) = hitl_runtime_for_worker.as_ref()
                {
                    registry
                        .register(Box::new(RequestInputTool::new(
                            hitl_runtime.clone(),
                            Arc::clone(&sink) as Arc<dyn ActivitySink>,
                        )))
                        .map_err(|e| format!("failed to register request_input tool: {e}"))?;
                }

                // 7a. Register py_exec when the `python` Cargo feature is
                // compiled in. Capability gating (`capability_token.python`)
                // happens inside the tool's `call` path, so we register
                // unconditionally here — agents without the capability get
                // a clean denial at invocation time rather than a phantom
                // missing-tool error.
                #[cfg(feature = "python")]
                registry
                    .register(Box::new(simulacra_python::PyExecTool::new(Arc::clone(
                        &cell,
                    ))))
                    .map_err(|e| format!("failed to register Python tool: {e}"))?;

                // 7b. Conditionally register memory tools (S037 §11). Opt-in
                // per agent type via capability_token.memory.enabled. Even
                // when the agent is memory-enabled, registration only
                // happens when the engine has all three memory handles
                // wired up. A `with_memory` engine is the typical path;
                // the default `new` / `with_components` engine leaves
                // memory disabled and the tools are not visible.
                if capability_token.memory.enabled
                    && let (Some(store), Some(index), Some(embedder)) = (
                        memory_store_opt.as_ref(),
                        vector_index_opt.as_ref(),
                        embedder_opt.as_ref(),
                    )
                    && let Ok(tenant_id) = simulacra_types::TenantId::parse(&tenant_namespace)
                {
                    register_memory_tools(
                        &mut registry,
                        MemoryToolHandles {
                            tenant: tenant_id,
                            capability: capability_token.memory.clone(),
                            memory_store: Arc::clone(store),
                            vector_index: Arc::clone(index),
                            embedder: Arc::clone(embedder),
                            hit_cache: Arc::clone(&hit_cache),
                            // Same RRWB Arc that MemoryStoreFs records to —
                            // closes the Guarantee 2 loop.
                            rrwb: Some(Arc::clone(&rrwb)),
                            hook_pipeline: Some(Arc::clone(&hook_pipeline)),
                        },
                    )
                    .map_err(|e| format!("failed to register memory tools: {e}"))?;
                }

                let skill_catalog = discover_and_filter_skills(
                    &vfs,
                    &agent_skill_names,
                    &capability_token,
                    &agent_type_name_owned,
                )
                .map_err(|e| format!("failed to discover catalog skills: {e}"))?;
                if skill_catalog
                    .iter()
                    .any(|skill| !skill.disable_model_invocation && skill.allow_implicit_invocation)
                {
                    registry
                        .register(Box::new(SkillTool::new(Arc::clone(&cell), skill_catalog)))
                        .map_err(|e| format!("failed to register Skill tool: {e}"))?;
                }

                if let Some(ref mcp_config) = config_for_worker.mcp {
                    let allowed_mcp_servers: Vec<&simulacra_config::McpServerConfig> = mcp_config
                        .servers
                        .iter()
                        .filter(|server| {
                            tenant_mcp_servers.contains(&server.name)
                                && mcp_capability_may_cover_server(
                                    &capability_token.mcp_tools,
                                    &server.name,
                                )
                        })
                        .collect();
                    let network_descriptors: Vec<(String, Option<String>, Option<String>)> =
                        allowed_mcp_servers
                            .iter()
                            .filter(|s| s.transport.as_deref() != Some("wasm"))
                            .map(|s| (s.name.clone(), s.url.clone(), s.transport.clone()))
                            .collect();
                    let wasm_descriptors: Vec<simulacra_mcp::WasmMcpServerDescriptor> =
                        allowed_mcp_servers
                            .iter()
                            .filter(|s| s.transport.as_deref() == Some("wasm"))
                            .filter_map(|s| {
                                let module = s.module.as_ref()?;
                                Some(simulacra_mcp::WasmMcpServerDescriptor {
                                    name: s.name.clone(),
                                    module_path: std::path::PathBuf::from(module),
                                    network_allowlist: s.network.clone(),
                                    hooks: Some(Arc::clone(&hook_pipeline)),
                                    journal: Some(Arc::clone(&journal)),
                                    agent_id: AgentId(task_id.clone()),
                                })
                            })
                            .collect();
                    let total_servers = network_descriptors.len() + wasm_descriptors.len();

                    if total_servers > 0 {
                        let mcp_tools = simulacra_mcp::create_mcp_tools_with_wasm(
                            &network_descriptors,
                            &wasm_descriptors,
                        )
                        .await;

                        tracing::info!(
                            task_id = %task_id,
                            mcp_tool_count = mcp_tools.len(),
                            mcp_server_count = total_servers,
                            "MCP tool discovery complete for server-launched agent"
                        );

                        for tool in mcp_tools.into_iter().filter(|tool| {
                            mcp_capability_covers_tool(
                                &capability_token.mcp_tools,
                                tool.server_name(),
                                tool.tool_name(),
                            )
                        }) {
                            registry
                                .register(Box::new(tool))
                                .map_err(|e| format!("failed to register MCP tool: {e}"))?;
                        }
                    }
                }

                let mut spawn_rx = None;
                let mut supervisor_tx_for_factory = None;
                if !capability_token.spawn_types.is_empty() {
                    let (spawn_tx, rx) = tokio::sync::mpsc::channel(16);
                    let spawn_sink: Arc<dyn ActivitySink> =
                        Arc::clone(&sink) as Arc<dyn ActivitySink>;
                    let spawn_tx_clone = spawn_tx.clone();
                    registry
                        .register(Box::new(SpawnAgentTool {
                            sender: spawn_tx,
                            can_spawn: capability_token.spawn_types.clone(),
                            activity_sink: spawn_sink,
                            parent_id: AgentId(task_id.clone()),
                            tiers: config_for_worker.tiers.clone(),
                            parent_budget: Arc::clone(&budget_arc),
                            parent_model: model_clone.clone(),
                        }))
                        .map_err(|e| format!("failed to register spawn_agent tool: {e}"))?;
                    spawn_rx = Some(rx);
                    supervisor_tx_for_factory = Some(spawn_tx_clone);
                }

                // 8. Populate shared tools (for ProcFs /proc/tools/)
                shared_tools.set(registry.definitions());

                if let Some(spawn_rx) = spawn_rx {
                    let supervisor_sink: Arc<dyn ActivitySink> =
                        Arc::clone(&sink) as Arc<dyn ActivitySink>;
                    let child_cell_configurator = integration_registry.as_ref().map(|reg| {
                        let reg = Arc::clone(reg);
                        let tenant_integrations = tenant_integrations.clone();
                        Arc::new(move |cell: &mut AgentCell| {
                            cell.integration_registry = Some(Arc::clone(&reg));
                            cell.tenant_integrations = tenant_integrations.clone();
                        }) as simulacra_runtime::ChildCellConfigurator
                    });
                    let child_tool_registrar: Option<simulacra_runtime::ChildToolRegistrar> = {
                        #[cfg(feature = "python")]
                        {
                            Some(Arc::new(
                                |registry: &mut simulacra_tool::ToolRegistry,
                                 cell: Arc<AgentCell>| {
                                    registry
                                        .register(Box::new(simulacra_python::PyExecTool::new(cell)))
                                },
                            ))
                        }
                        #[cfg(not(feature = "python"))]
                        {
                            None
                        }
                    };
                    let task_factory = Arc::new(AgentTaskFactory {
                        config: config_for_worker.clone(),
                        provider_kind: runtime_provider_kind,
                        vfs: Arc::clone(&vfs),
                        journal: Arc::clone(&journal),
                        activity_sink: supervisor_sink,
                        parent_capability: capability_token.clone(),
                        supervisor_sender: supervisor_tx_for_factory,
                        parent_model: model_clone.clone(),
                        pipeline: Some(Arc::clone(&hook_pipeline)),
                        script_executor: Some(ScriptExecutor::new(4)),
                        child_cell_configurator,
                        child_tool_registrar,
                    });
                    let mut supervisor = AgentSupervisor::with_task_factory(
                        capability_token.clone(),
                        budget_arc.lock().unwrap().clone(),
                        task_factory,
                    );
                    supervisor.set_activity_sink(Arc::clone(&sink) as Arc<dyn ActivitySink>);
                    tokio::spawn(async move {
                        supervisor.run_actor_loop(spawn_rx).await;
                    });
                }

                // 9. Build provider — production path constructs in-band
                // from env vars; S043 override path delegates to the
                // installed factory.
                let provider: Box<dyn simulacra_types::Provider> =
                    if let Some(factory) = provider_factory_for_worker.as_ref() {
                        factory(provider_kind, &model_clone)
                            .map_err(|e| format!("provider factory failed: {e}"))?
                    } else {
                        let api_key = match provider_kind {
                            ProviderKind::Anthropic => std::env::var("ANTHROPIC_API_KEY")
                                .map_err(|_| "ANTHROPIC_API_KEY not set".to_string())?,
                            ProviderKind::OpenAI => std::env::var("OPENAI_API_KEY")
                                .map_err(|_| "OPENAI_API_KEY not set".to_string())?,
                            ProviderKind::Ollama => "ollama".to_string(),
                        };
                        match provider_kind {
                            ProviderKind::Anthropic => {
                                Box::new(AnthropicProvider::new(&api_key, &model_clone))
                                    as Box<dyn simulacra_types::Provider>
                            }
                            ProviderKind::OpenAI | ProviderKind::Ollama => {
                                Box::new(OpenAiProvider::new(&api_key, &model_clone))
                                    as Box<dyn simulacra_types::Provider>
                            }
                        }
                    };

                // 10. Build AgentLoop
                let config = AgentLoopConfig {
                    agent_id: AgentId(task_id.clone()),
                    system_prompt: system_prompt.clone(),
                    model: model_clone.clone(),
                    max_turns,
                    capability: capability_token,
                };
                let strategy = Box::new(ObservationMaskingStrategy::new(3))
                    as Box<dyn simulacra_types::ContextStrategy>;
                let budget_for_loop = budget_arc.lock().unwrap().clone();
                let mut agent_loop = AgentLoop::new(
                    config,
                    provider,
                    registry,
                    strategy,
                    journal,
                    budget_for_loop,
                    Some(Arc::clone(&sink) as Arc<dyn ActivitySink>),
                    None, // no hook pipeline for now
                );
                agent_loop.set_proc_budget_mirror(Arc::clone(&budget_arc), Arc::clone(&proc_turn));
                if let Some(hitl_runtime) = hitl_runtime_for_worker {
                    agent_loop.set_hitl_runtime(hitl_runtime);
                }

                // 11. Run the agent with cancellation support
                let result = tokio::select! {
                    result = agent_loop.run(&description_owned) => result,
                    () = cancel_token.cancelled() => {
                        Ok(AgentLoopOutput {
                            exit_reason: simulacra_types::ExitReason::Cancelled,
                            messages: vec![],
                            token_usage: Default::default(),
                            used_turns: 0,
                            used_cost: Decimal::ZERO,
                        })
                    }
                };

                match result {
                    Ok(output) => Ok(output.exit_reason),
                    Err(e) => Err(e.to_string()),
                }
            }); // end rt.block_on(async move { ... })
            let _ = result_tx.send(result);
        }))?; // end pool.submit(Box::new(move || { ... }))

        // Spawn a wrapper task that handles panics (oneshot dropped) and maps results to TaskState.
        let task_id_for_completion = handle.task_id.clone();
        tokio::spawn(async move {
            let task_id = task_id_for_completion;
            match result_rx.await {
                Ok(Ok(exit_reason)) => {
                    let (state, reason) = map_exit_reason(&exit_reason);
                    let _ = task_manager_clone.complete_task(&task_id, state, reason);
                }
                Ok(Err(runtime_error)) => {
                    tracing::warn!(task_id = %task_id, error = %runtime_error, "agent task failed");
                    let _ = task_manager_clone.complete_task(
                        &task_id,
                        TaskState::Failed,
                        Some(runtime_error),
                    );
                }
                Err(_recv_err) => {
                    // Oneshot sender dropped — worker panicked.
                    tracing::error!(task_id = %task_id, "agent task panicked (oneshot dropped)");
                    let _ = task_manager_clone.complete_task(
                        &task_id,
                        TaskState::Failed,
                        Some("agent task panicked".into()),
                    );
                }
            }
        });

        Ok(handle)
    }

    /// Return the workspace MemoryFs for the given task (for testing/debugging).
    ///
    /// This is the raw MemoryFs that was created when the task was spawned,
    /// before the ServiceFs/ProcFs wrappers. Useful for asserting that file
    /// attachments were seeded correctly.
    pub fn debug_workspace_snapshot(&self, task_id: &str) -> Option<Arc<MemoryFs>> {
        let snapshots = self.workspace_snapshots.lock().unwrap();
        snapshots.get(task_id).cloned()
    }
}

/// Build a `CapabilityToken` from a resolved catalog agent.
///
/// `ResolvedAgent.capabilities` is a `Vec<String>` of strings such as
/// `"shell:exec"`, `"javascript"`, `"python"`, `"net:read:host"`, or
/// `"mcp:tool-name"`. Unknown strings are ignored. Boolean flags default
/// to false (locked-down), and `paths_read` / `paths_write` default to
/// `/**` so the agent can still touch the workspace until path
/// capabilities are persisted in the catalog (S042 §"Capability storage").
fn build_capability_token_from_resolved(resolved: &ResolvedAgent) -> CapabilityToken {
    use simulacra_types::{NetworkPermission, PathPattern};

    let mut shell = false;
    let mut javascript = false;
    let mut python = false;
    let mut network: Vec<NetworkPermission> = Vec::new();
    let mut mcp_tools: Vec<String> = Vec::new();
    let mut spawn_types: Vec<String> = Vec::new();
    let mut skill_patterns: Vec<String> = Vec::new();

    for cap in &resolved.capabilities {
        let trimmed = cap.trim();
        if trimmed.is_empty() {
            continue;
        }
        match trimmed {
            "shell" | "shell:exec" => shell = true,
            "javascript" | "js" => javascript = true,
            "python" | "py" => python = true,
            other if other.starts_with("net:") => {
                network.push(NetworkPermission(other.to_owned()));
            }
            other if other.starts_with("mcp:") => {
                if let Some(pattern) = normalize_mcp_capability(other) {
                    mcp_tools.push(pattern);
                }
            }
            other if other.starts_with("skill:") => {
                skill_patterns.push(other.to_owned());
            }
            other if other.starts_with("spawn:") => {
                let agent_type = other.trim_start_matches("spawn:").trim();
                if !agent_type.is_empty() {
                    spawn_types.push(agent_type.to_owned());
                }
            }
            _ => {}
        }
    }

    CapabilityToken {
        shell,
        javascript,
        python,
        network,
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![PathPattern("/**".into())],
        mcp_tools,
        spawn_types,
        skill_patterns,
        memory: simulacra_types::MemoryCapability::default(),
    }
}

fn read_only_spawn_snapshot_paths(vfs: Arc<dyn VirtualFs>) -> Arc<dyn VirtualFs> {
    ["/skills", "/var/skills", "/var/agent_files"]
        .into_iter()
        .fold(vfs, |inner, prefix| {
            Arc::new(ReadOnlyPathGuard::new(inner, prefix)) as Arc<dyn VirtualFs>
        })
}

fn validate_catalog_skill_name_for_vfs(name: &str) -> Result<(), EngineError> {
    if CatalogSkillFs::is_valid_skill_path_name(name) {
        Ok(())
    } else {
        Err(EngineError::VfsError(format!(
            "invalid catalog skill name for VFS path segment: {name:?}"
        )))
    }
}

fn runtime_provider_kind(kind: ProviderKind) -> simulacra_runtime::ProviderKind {
    match kind {
        ProviderKind::Anthropic => simulacra_runtime::ProviderKind::Anthropic,
        ProviderKind::OpenAI => simulacra_runtime::ProviderKind::OpenAI,
        ProviderKind::Ollama => simulacra_runtime::ProviderKind::Ollama,
    }
}

fn normalize_mcp_capability(capability: &str) -> Option<String> {
    let rest = capability.strip_prefix("mcp:")?.trim();
    if rest.is_empty() {
        return None;
    }
    let segment_count = rest.split(':').count();
    match segment_count {
        1 => Some(format!("mcp:{rest}:*")),
        _ => Some(format!("mcp:{rest}")),
    }
}

fn mcp_capability_may_cover_server(patterns: &[String], server: &str) -> bool {
    patterns.iter().any(|pattern| {
        let Some(rest) = pattern.strip_prefix("mcp:") else {
            return false;
        };
        let Some((server_pattern, tool_pattern)) = rest.split_once(':') else {
            return false;
        };
        !tool_pattern.is_empty() && (server_pattern == "*" || server_pattern == server)
    })
}

fn mcp_capability_covers_tool(patterns: &[String], server: &str, tool: &str) -> bool {
    let qualified = format!("mcp:{server}:{tool}");
    patterns.iter().any(|pattern| {
        let Some(rest) = pattern.strip_prefix("mcp:") else {
            return false;
        };
        let Some((server_pattern, tool_pattern)) = rest.split_once(':') else {
            return false;
        };
        !server_pattern.is_empty()
            && !tool_pattern.is_empty()
            && mcp_pattern_matches(pattern, &qualified)
    })
}

fn mcp_pattern_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let pattern_bytes = pattern.as_bytes();
    let value_bytes = value.as_bytes();
    let (mut pattern_i, mut value_i) = (0, 0);
    let mut star_i = None;
    let mut value_after_star = 0;

    while value_i < value_bytes.len() {
        if pattern_i < pattern_bytes.len()
            && (pattern_bytes[pattern_i] == value_bytes[value_i]
                || pattern_bytes[pattern_i] == b'?')
        {
            pattern_i += 1;
            value_i += 1;
        } else if pattern_i < pattern_bytes.len() && pattern_bytes[pattern_i] == b'*' {
            star_i = Some(pattern_i);
            pattern_i += 1;
            value_after_star = value_i;
        } else if let Some(star) = star_i {
            pattern_i = star + 1;
            value_after_star += 1;
            value_i = value_after_star;
        } else {
            return false;
        }
    }

    while pattern_i < pattern_bytes.len() && pattern_bytes[pattern_i] == b'*' {
        pattern_i += 1;
    }
    pattern_i == pattern_bytes.len()
}

/// Build a fresh in-memory `Catalog` and seed it from `SimulacraConfig` so the
/// engine can run unchanged against TOML-only configs.
async fn build_in_memory_catalog(
    config: &SimulacraConfig,
) -> Result<
    (
        Arc<dyn AgentRepository>,
        Arc<dyn SkillRepository>,
        Arc<dyn MemoryPoolRepository>,
        Arc<dyn TenantRepository>,
    ),
    EngineError,
> {
    let catalog = Catalog::open_in_memory()?;
    let tenants_repo = catalog.tenants();
    let agents_repo = catalog.agents();

    // Build (tenant_namespace -> agent_type_name) pairs that need rows in the
    // catalog. With multi-tenant configs the same agent_type can be referenced
    // by N tenants; each tenant must get its own row.
    let mut to_seed: Vec<(String, String)> = Vec::new();
    if config.tenants.is_empty() {
        for name in config.agent_types.keys() {
            to_seed.push(("default".to_string(), name.clone()));
        }
    } else {
        for (ns, t_cfg) in &config.tenants {
            to_seed.push((ns.clone(), t_cfg.agent_type.clone()));
        }
    }

    let mut tenant_ids: HashMap<String, simulacra_catalog::TenantId> = HashMap::new();
    for (ns, _) in &to_seed {
        if !tenant_ids.contains_key(ns) {
            let row = tenants_repo
                .get_or_create(ns, Some(ns))
                .await
                .map_err(EngineError::from)?;
            tenant_ids.insert(ns.clone(), row.id);
        }
    }

    for (ns, agent_name) in &to_seed {
        let Some(agent_cfg) = config.agent_types.get(agent_name) else {
            continue;
        };
        let Some(tenant_id) = tenant_ids.get(ns) else {
            continue;
        };
        let capabilities = agent_capabilities_from_config(agent_cfg);
        let skill_ids: Vec<SkillId> = Vec::new();
        let _ = agents_repo
            .create(
                tenant_id,
                NewAgent {
                    name: agent_name,
                    description: None,
                    system_prompt: agent_cfg.system_prompt.as_deref().unwrap_or(""),
                    model: &agent_cfg.model,
                    max_turns: agent_cfg.max_turns,
                    max_tokens: agent_cfg.max_tokens.map(|n| n as u32),
                    memory_pool_id: None,
                    skill_ids: &skill_ids,
                    capabilities: &capabilities,
                    channel_ids: &[],
                },
            )
            .await;
    }

    Ok((
        Arc::new(catalog.agents()) as Arc<dyn AgentRepository>,
        Arc::new(catalog.skills()) as Arc<dyn SkillRepository>,
        Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>,
        Arc::new(catalog.tenants()) as Arc<dyn TenantRepository>,
    ))
}

/// Convert TOML capability config into the catalog's `Vec<String>` form.
fn agent_capabilities_from_config(agent: &simulacra_config::AgentTypeConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(caps) = agent.capabilities.as_ref() {
        if caps.shell {
            out.push("shell:exec".into());
        }
        if caps.javascript {
            out.push("javascript".into());
        }
        if caps.python {
            out.push("python".into());
        }
        for n in &caps.network {
            out.push(if n.starts_with("net:") {
                n.clone()
            } else {
                format!("net:{n}")
            });
        }
        for m in &caps.mcp {
            out.push(if m.starts_with("mcp:") {
                m.clone()
            } else {
                format!("mcp:{m}")
            });
        }
    }
    for agent_type in &agent.can_spawn {
        out.push(format!("spawn:{agent_type}"));
    }
    out
}
