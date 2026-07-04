# S034 — SimulacraEngine: API-Triggered Agent Execution

**Status:** Active
**Crates:** `simulacra-server` (primary), `simulacra-runtime`, `simulacra-sandbox`, `simulacra-tool`, `simulacra-vfs`, `simulacra-hooks`, `simulacra-config`, `simulacra-integration`, `simulacra-provider`, `simulacra-mcp`

## Dependencies

- **S001** — Virtual filesystem (VFS composition: MemoryFs, OverlayFs, host mounts)
- **S006** — Resource budgets (per-task budget creation and enforcement)
- **S011** — Sandbox composition (AgentCell construction, Golden Rule enforcement)
- **S012** — Built-in tools (ToolRegistry, register_builtins)
- **S017** — Skills (skill discovery, SkillTool registration)
- **S019** — Activity events (ActivitySink trait, ActivityEvent enum, event bridging)
- **S026** — Governance hooks (HookPipeline construction from config)
- **S029** — Agent procfs (ProcFs layer, ProcState, SharedToolList)
- **S031** — API server (TaskManager, TaskState, TaskHandle, per-task broadcast, WebSocket/REST+SSE)
- **S033** — Integration fabric (IntegrationRegistry, ServiceFs, CredentialInjector)

## Scope

SimulacraEngine is the composition root that bridges the API server to the Simulacra runtime. It constructs fully-configured agents from tenant configuration and spawns them on background tokio tasks, piping activity events back through the TaskManager's per-task broadcast channel.

**In scope:**
- `SimulacraEngine` struct replacing the empty stub in `simulacra-server/src/server.rs`
- Per-task agent construction: VFS (MemoryFs + host mounts + ServiceFs + ProcFs), AgentCell, ToolRegistry, HookPipeline, ResourceBudget, Journal
- Catalog-backed skill snapshots: server tasks expose catalog skills through read-only canonical `/skills/<name>/SKILL.md` files for S017 discovery, plus read-only `/var/skills/<name>.md` compatibility files
- `EngineActivitySink` — translates `ActivityEvent` to server event JSON, sends through per-task `broadcast::Sender<Value>` (no lock acquisition)
- Background tokio task spawning for agent execution with panic safety
- Agent completion/failure/budget exhaustion mapped to terminal TaskState transitions
- `TaskManager::emit_event` — public method to push events on a task's broadcast channel with seq increment
- Provider resolution extracted from CLI into shared function
- `ExitReason` to `TaskState` mapping (all variants including `AwaitingApproval`)
- `EngineError` typed error enum
- `ChildActivity` flattening with child attribution
- `CancellationToken` wiring for `task.cancel`
- WASM and Python tool registration (feature-gated)

**Out of scope:**
- CLI refactoring to use SimulacraEngine (follow-up)
- Full `input.response` / `approval.respond` plumbing into the AgentLoop (requires AgentLoop changes; interfaces defined here, integration is follow-up)
- Agent-to-agent task spawning via SimulacraEngine (uses existing supervisor/SpawnAgentTool)
- WebSocket reconnection / session resume
- MicroVM / fork isolation backends
- Workflow hardening, curator agent
- Per-tenant hook filtering (hooks are global from `SimulacraConfig.hooks`)

## Context

Today, `simulacra-cli/src/lib.rs` has ~400 lines of agent construction logic inline: VFS layering, journal creation, hook pipeline setup, AgentCell wiring, ToolRegistry population (builtins, skills, MCP, WASM, Python), provider inference, and AgentLoop construction. This logic is CLI-specific — parameterized by `CliArgs`.

The API server (`simulacra-server`) has a `SimulacraEngine` that is an empty `#[derive(Default)]` struct. `TaskManager` manages task state transitions and per-task broadcast channels, but `create_task` does not actually spawn an agent — it immediately transitions to `Running` as a placeholder.

SimulacraEngine closes this gap. It performs the same agent construction as the CLI bootstrap, but parameterized by server-side `TenantConfig` (from `simulacra-server::tenant`) and `SimulacraConfig` (loaded at server startup). Each task creation constructs an isolated agent environment, spawns the `AgentLoop` on a tokio task, and bridges events back through the TaskManager.

The key architectural insight is that SimulacraEngine lives in `simulacra-server` and depends on everything — it is intentionally the composition root.

### Two TenantConfig types

`simulacra-server::tenant::TenantConfig` and `simulacra-config::TenantConfig` are different types. SimulacraEngine uses **`simulacra-server::tenant::TenantConfig`** which contains:

```rust
// simulacra-server/src/tenant.rs
pub struct TenantConfig {
    pub namespace: String,
    pub agent_type: String,
    pub vfs_root: PathBuf,
    pub budget_pool: BudgetPoolConfig,
    pub hooks: Vec<String>,         // hook names (not full HookConfig)
}
```

SimulacraEngine also consumes `AgentTypeConfig` from `SimulacraConfig.agent_types` (in `simulacra-config`) to resolve model, system prompt, capabilities, max_turns, and skills for each task. The `SimulacraConfig` is stored in the engine at construction time.

## Design

### SimulacraEngine

```rust
pub struct SimulacraEngine {
    config: SimulacraConfig,
    integration_registry: Option<Arc<IntegrationRegistry>>,
}

impl SimulacraEngine {
    /// Construct the engine. Returns an error if validation fails
    /// (e.g., tenant-referenced agent types missing from config).
    pub fn new(
        config: SimulacraConfig,
        integration_registry: Option<Arc<IntegrationRegistry>>,
    ) -> Result<Self, EngineError>;

    /// Create a task, construct the agent, spawn it, and return the task handle.
    ///
    /// This method owns the full lifecycle:
    /// 1. Calls `TaskManager::create_task(...)` which creates a Pending task
    ///    and immediately transitions to Running (existing behavior).
    /// 2. Extracts the `broadcast::Sender<Value>` from the TaskManager for
    ///    this task (used by EngineActivitySink).
    /// 3. Constructs the agent (VFS, tools, provider, etc.).
    /// 4. Spawns the agent on a background tokio task.
    /// 5. Returns the TaskHandle immediately.
    ///
    /// If agent construction fails after task creation, the task is
    /// transitioned to Failed before returning the error.
    pub async fn spawn_task(
        &self,
        task_manager: &TaskManager,
        description: &str,
        tenant: &TenantConfig,
        agent_type_override: Option<&str>,
        metadata: Value,
        connection_id: Option<String>,
    ) -> Result<TaskHandle, EngineError>;
}
```

### EngineError

```rust
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("agent type '{0}' not found in config")]
    AgentTypeNotFound(String),
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
    #[error("internal error: {0}")]
    Internal(String),
}
```

### EngineActivitySink

```rust
/// Non-blocking activity sink that translates ActivityEvents to server JSON
/// and sends them on the task's broadcast channel.
///
/// Stores a cloned `broadcast::Sender<Value>` directly — no lock acquisition
/// in the hot path. The sender is extracted at task creation time from
/// TaskManager and cloned into this struct.
struct EngineActivitySink {
    task_id: String,
    sender: broadcast::Sender<Value>,
    /// Monotonic sequence counter for this task's events.
    seq: AtomicU64,
}

impl ActivitySink for EngineActivitySink {
    fn emit(&self, event: ActivityEvent) {
        // Flatten ChildActivity recursively, then translate.
        let events = flatten_activity_event(&self.task_id, &event);
        for server_event in events {
            let seq = self.seq.fetch_add(1, Ordering::Relaxed);
            let mut evt = server_event;
            evt["seq"] = serde_json::Value::from(seq);
            let _ = self.sender.send(evt);
        }
    }
}
```

Key properties:
- `emit()` calls `broadcast::Sender::send()` which is non-blocking (returns `Err` if no receivers, which is silently dropped).
- No `Mutex`, no `TaskManager` reference, no lock acquisition on the hot path.
- Each event gets a monotonic `seq` number via `AtomicU64`.

### ChildActivity flattening

`ChildActivity` events are recursive wrappers emitted by `ForwardingActivitySink` when child agents produce events. `flatten_activity_event` unwraps them:

```rust
fn flatten_activity_event(task_id: &str, event: &ActivityEvent) -> Vec<Value> {
    match event {
        ActivityEvent::ChildActivity { child_id, agent_type, event: inner } => {
            // Recursively flatten. The innermost event gets a `child_id` field.
            let mut flattened = flatten_activity_event(task_id, inner);
            for evt in &mut flattened {
                // Add child attribution. If already present (nested), build
                // a chain: grandchild → child → parent.
                if evt.get("child_id").is_none() {
                    evt["child_id"] = serde_json::Value::from(child_id.clone());
                    evt["child_agent_type"] = serde_json::Value::from(agent_type.clone());
                }
            }
            flattened
        }
        other => {
            vec![translate_activity_event(task_id, other)]
        }
    }
}
```

All events from children appear in the parent's event stream with `child_id` and `child_agent_type` fields added. Deeply nested children preserve the innermost child attribution (the direct producer).

### Event translation mapping

| ActivityEvent variant | Server event JSON |
|---|---|
| `Token { text }` | `{ event: "agent.message", task_id, content: text, role: "assistant", seq }` |
| `ThinkStart` | `{ event: "agent.thinking", task_id, state: "started", seq }` |
| `ThinkDelta { text }` | `{ event: "agent.thinking", task_id, content: text, seq }` |
| `ThinkEnd { think_duration_ms, think_tokens }` | `{ event: "agent.thinking", task_id, state: "ended", duration_ms, tokens, seq }` |
| `ToolStart { tool_call_id, name, arguments }` | `{ event: "tool.called", task_id, tool_call_id, tool_name, arguments, seq }` |
| `ToolApprovalRequired { tool_call_id, name, arguments, reason }` | `{ event: "tool.approval_required", task_id, tool_call_id, tool_name, arguments, reason, seq }` |
| `ToolCallDelta { index, tool_call_id, name, arguments_delta }` | `{ event: "tool.call_delta", task_id, index, tool_call_id, tool_name, arguments_delta, seq }` |
| `ToolOutput { tool_call_id, line }` | `{ event: "tool.output", task_id, tool_call_id, line, seq }` |
| `InputRequired { prompt, schema }` | `{ event: "input.required", task_id, prompt, schema, seq }` |
| `ToolFinish { tool_call_id, name, is_error, duration_ms, .. }` | `{ event: "tool.result", task_id, tool_call_id, tool_name, is_error, duration_ms, seq }` |
| `ChildSpawned { child_id, agent_type, task }` | `{ event: "agent.child_spawned", task_id, child_id, agent_type, child_task: task, seq }` |
| `ChildFinished { child_id, agent_type, exit_reason, duration_ms, .. }` | `{ event: "agent.child_finished", task_id, child_id, agent_type, exit_reason, duration_ms, seq }` |
| `ChildActivity { .. }` | Flattened (see above) — inner event emitted with `child_id` field |
| `TurnComplete` | `{ event: "agent.turn_complete", task_id, seq }` |

**Note on S031 event schema:** S031 defines `tool.called`, `tool.result`, `agent.message`, `agent.thinking` as server events. This spec adds `tool.output`, `agent.child_spawned`, `agent.child_finished`, and `agent.turn_complete` which are not yet in S031. S031 should be updated as a follow-up to include these event types. S034 is the authoritative source for the full runtime event catalog since it has direct knowledge of what the runtime emits.

### ExitReason to TaskState mapping

| ExitReason | TaskState | reason field |
|---|---|---|
| `Complete` | `Completed` | `None` |
| `MaxTurns` | `Completed` | `Some("max_turns")` |
| `BudgetExhausted` | `Killed` | `Some("budget_exhausted")` |
| `Cancelled` | `Cancelled` | `None` |
| `GuardrailTripped(msg)` | `Killed` | `Some(format!("guardrail: {msg}"))` |
| `PolicyKill { hook, reason }` | `Killed` | `Some(format!("policy: {hook}: {reason}"))` |
| `Error(msg)` | `Failed` | `Some(msg)` |
| `AwaitingApproval` | `WaitingApproval` | `None` (non-terminal; task pauses, awaits `approval.respond`) |

`AwaitingApproval` is preserved for compatibility, but S051 implements the
server-launched HITL path without exiting the loop: the agent emits
`ToolApprovalRequired`, the engine transitions to `WaitingApproval`, and
`approval.respond` resumes the same live loop.

### complete_task extension

```rust
impl TaskManager {
    /// Mark a task as terminal. The `reason` is included in the
    /// `task.state_changed` event.
    pub fn complete_task(
        &self,
        task_id: &str,
        terminal_state: TaskState,
        reason: Option<String>,
    ) -> Result<(), TaskManagerError>;
}
```

The existing `complete_task` signature gains an `Option<String>` reason parameter. The reason is included in the `task.state_changed` event JSON:

```json
{
    "event": "task.state_changed",
    "task_id": "...",
    "from": "running",
    "to": "completed",
    "reason": "max_turns",
    "seq": 42
}
```

### TaskManager emit_event

```rust
impl TaskManager {
    /// Push an event onto a task's broadcast channel.
    ///
    /// Increments the task's monotonic seq counter. Every event — not just
    /// state transitions — gets a seq number. This aligns with S031's
    /// requirement that events are ordered per-task with monotonic sequence
    /// numbers.
    pub fn emit_event(&self, task_id: &str, event: Value) -> Result<u64, TaskManagerError>;
}
```

Returns the assigned seq number on success. Acquires the task lock briefly to increment seq and send. Returns `NotFound` for nonexistent task. Silently drops if no subscribers (broadcast send returns Err, which is ignored).

**Note:** `EngineActivitySink` does NOT use `emit_event` — it sends directly on a cloned `broadcast::Sender<Value>` with its own `AtomicU64` seq counter. `emit_event` is for state-change events and other events emitted by the TaskManager itself (budget warnings, etc.).

### TaskManager broadcast sender extraction

```rust
impl TaskManager {
    /// Get the broadcast sender for a task. Used by SimulacraEngine to construct
    /// EngineActivitySink without ongoing lock acquisition.
    pub fn get_event_sender(&self, task_id: &str) -> Result<broadcast::Sender<Value>, TaskManagerError>;
}
```

### Provider resolution (shared)

Extract a shared `build_provider` function into `simulacra-runtime` (or a new `simulacra-provider-factory` module) that all three call sites use:

```rust
/// Build an LLM provider from model name.
///
/// Infers ProviderKind from the model string, then constructs the
/// appropriate provider with API keys from environment.
pub fn build_provider(model: &str) -> Result<Box<dyn Provider>, EngineError> {
    let kind = infer_provider_kind(model)?;
    match kind {
        ProviderKind::Anthropic => {
            let key = std::env::var("ANTHROPIC_API_KEY")
                .map_err(|_| EngineError::MissingEnvVar("ANTHROPIC_API_KEY".into()))?;
            Ok(Box::new(AnthropicProvider::new(key, model)))
        }
        ProviderKind::OpenAI => {
            let key = std::env::var("OPENAI_API_KEY")
                .map_err(|_| EngineError::MissingEnvVar("OPENAI_API_KEY".into()))?;
            Ok(Box::new(OpenAiProvider::new(key, model)))
        }
        ProviderKind::Ollama => {
            Ok(Box::new(OpenAiProvider::new("ollama", model)))
        }
    }
}

pub fn infer_provider_kind(model: &str) -> Result<ProviderKind, EngineError> {
    if model.starts_with("claude-") {
        Ok(ProviderKind::Anthropic)
    } else if model.starts_with("ollama:") {
        Ok(ProviderKind::Ollama)
    } else {
        // Default: OpenAI-compatible (covers gpt-*, groq models, etc.)
        Ok(ProviderKind::OpenAI)
    }
}
```

The CLI's `infer_provider_kind` and `build_provider` are refactored to call this shared function. The CLI and `spawn_tool.rs` remain as call sites but delegate to the shared implementation.

### Cancellation wiring

```rust
// At spawn time, SimulacraEngine creates a CancellationToken:
let cancellation = CancellationToken::new(Duration::from_secs(30));

// Store a clone in the TaskManager (new field on TaskRecord):
// cancellation_token: Option<CancellationToken>

// When task.cancel arrives:
// 1. TaskManager retrieves the CancellationToken for the task
// 2. Calls cancellation.signal()
// 3. The agent loop checks cancellation before provider calls and during tool dispatch,
//    then exits with ExitReason::Cancelled or a cancelled tool result per S049
// 4. The background task completes, triggering the terminal state transition
```

`TaskManager` gains:
```rust
impl TaskManager {
    /// Store a cancellation token for a task (called by SimulacraEngine after spawn).
    pub fn set_cancellation_token(&self, task_id: &str, token: CancellationToken) -> Result<(), TaskManagerError>;
}
```

`cancel_task` is updated to signal the token before transitioning state. The actual state transition to `Cancelled` happens when the background agent task observes cancellation and exits. S049 owns the shared `AgentLoop` cancellation checks and tool-dispatch cancellation result shape.

### Input and approval channels

For `input.response` and `approval.respond`, S051 wires live channels into the
agent task:

```rust
// TaskRecord gains:
// input_tx: Option<tokio::sync::mpsc::Sender<String>>
// approval_tx: Option<tokio::sync::mpsc::Sender<ToolApprovalResponse>>
```

SimulacraEngine creates these channels for HITL-enabled tasks and passes the
receivers to the agent task. `provide_input` and `respond_approval` validate
the waiting state, send on the matching channel, and transition the task back
to `Running`.

### Budget warnings

The engine monitors budget usage by checking the `ResourceBudget` state. Budget warning events are emitted at 80% and 95% thresholds:

```json
{
    "event": "budget.warning",
    "task_id": "...",
    "budget_type": "tokens",
    "used": 8000,
    "limit": 10000,
    "pct": 80,
    "seq": 37
}
```

Budget checking happens in `EngineActivitySink::emit()` on `TurnComplete` events — the sink checks the shared `ResourceBudget` and emits warnings if thresholds are crossed. Alternatively, the background agent task checks after each turn. The implementation may choose either approach; the observable behavior is that warnings are emitted at the specified thresholds.

### Per-task agent construction sequence

1. Resolve `AgentTypeConfig` from `SimulacraConfig.agent_types` using tenant's `agent_type` or override
2. Call `TaskManager::create_task(...)` — creates task in Pending, transitions to Running, returns TaskHandle
3. Extract `broadcast::Sender<Value>` via `TaskManager::get_event_sender(&task_id)`
4. Build `CapabilityToken` from agent type's `CapabilitiesConfig`
5. Build `ResourceBudget` from tenant's `BudgetPoolConfig`
6. Create `MemoryFs`, seed `/workspace/task.md` with description, validate catalog skill names as single VFS path segments, and snapshot catalog-authored skills into read-only `/skills/<name>/SKILL.md` and `/var/skills/<name>.md`
7. Process VFS host mounts from tenant's `vfs_root`
8. Wrap VFS: `MemoryFs` → `ServiceFs` (scoped to tenant integrations) → `ProcFs`
9. Create `HookPipeline` from `SimulacraConfig.hooks` (global, not per-tenant)
10. Create `AgentCell` with VFS, capabilities, budget, journal, HTTP client, integration registry
11. Create `ToolRegistry`, register builtins + the single S017 `Skill` tool when model-visible catalog skills remain + MCP tools + WASM tools (feature-gated) + Python tools (feature-gated)
12. Create `EngineActivitySink` with cloned broadcast sender and AtomicU64 seq counter
13. Build `AgentLoopConfig` { agent_id, system_prompt, model, max_turns, capability }
14. Call `build_provider(model)` to construct the LLM provider
15. Construct `AgentLoop::new(config, provider, tools, context_strategy, journal, budget, hook_pipeline, activity_sink)`
16. Create `CancellationToken`, store in TaskManager via `set_cancellation_token`
17. `tokio::spawn` the agent task with panic safety (see below); return TaskHandle immediately

If any step 4-16 fails, transition the task to `Failed` via `TaskManager::complete_task(task_id, TaskState::Failed, Some(error_message))` and return the error.

### Panic handling

The spawned tokio task wraps the agent future with `AssertUnwindSafe` + `catch_unwind`:

```rust
tokio::spawn(async move {
    let result = AssertUnwindSafe(agent_loop.run(&description))
        .catch_unwind()
        .await;

    match result {
        Ok(Ok(output)) => {
            // Map ExitReason to TaskState
            let (state, reason) = map_exit_reason(&output.exit_reason);
            let _ = task_manager.complete_task(&task_id, state, reason);
        }
        Ok(Err(runtime_error)) => {
            let _ = task_manager.complete_task(
                &task_id,
                TaskState::Failed,
                Some(runtime_error.to_string()),
            );
        }
        Err(_panic) => {
            tracing::error!(task_id = %task_id, "agent task panicked");
            let _ = task_manager.complete_task(
                &task_id,
                TaskState::Failed,
                Some("agent task panicked".into()),
            );
        }
    }
});
```

This ensures panics in the agent loop are caught and the task transitions to `Failed` instead of leaving the task permanently in `Running`.

### Server startup

`start_server` is updated to accept `SimulacraConfig` and construct `SimulacraEngine`:

```rust
pub async fn start_server(
    config: ServerConfig,
    simulacra_config: SimulacraConfig,
    auth: Arc<dyn AuthProvider>,
    resolver: TenantResolver,
) -> Result<(), ServerError> {
    let integration_registry = build_integration_registry(&simulacra_config);
    let engine = SimulacraEngine::new(simulacra_config, integration_registry)?;
    let task_manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(resolver);
    let state = AppState::with_engine(task_manager, resolver, auth, Arc::new(engine));
    let server = SimulacraServer::new(config, state, vec![]);
    server.run().await
}
```

`AppState` stores `Arc<SimulacraEngine>` (replacing the empty stub). The `create_task` REST/WS handlers call `engine.spawn_task(...)` instead of `task_manager.create_task(...)` directly.

### Event delivery semantics

SSE subscribers joining after task creation receive events from the point of subscription, not from task start. The initial `pending -> running` transition event may be missed if no subscriber exists when the task is created. This is intentional — the event stream is best-effort streaming, not a durable event log. Clients that need the full history should query `GET /api/v1/tasks/{task_id}` for current state.

### Hook selection

Hooks are loaded globally from `SimulacraConfig.hooks` (not per-tenant). All tasks share the same hook pipeline. The tenant's `hooks: Vec<String>` field in `TenantConfig` is reserved for future per-tenant hook filtering but is not used in S034.

### Required crate dependencies

The following must be added to `simulacra-server/Cargo.toml`:

```toml
[dependencies]
# Existing deps unchanged, add:
simulacra-config = { path = "../simulacra-config" }
simulacra-runtime = { path = "../simulacra-runtime" }
simulacra-sandbox = { path = "../simulacra-sandbox" }
simulacra-tool = { path = "../simulacra-tool" }
simulacra-vfs = { path = "../simulacra-vfs" }
simulacra-hooks = { path = "../simulacra-hooks" }
simulacra-types = { path = "../simulacra-types" }
simulacra-provider = { path = "../simulacra-provider" }
simulacra-integration = { path = "../simulacra-integration" }
simulacra-mcp = { path = "../simulacra-mcp" }
rust-decimal = { version = "1", features = ["serde-with-str"] }
```

## Behavior

### Engine construction
1. `SimulacraEngine::new` receives global `SimulacraConfig` and optional `IntegrationRegistry`.
2. Validates that config is internally consistent (e.g., agent types referenced by tenants exist in `config.agent_types`).
3. Validation failure returns `Err(EngineError)`, not panic.
4. Stores `SimulacraConfig` and `IntegrationRegistry` for use in task spawning.

### Task spawning
5. `spawn_task` resolves `AgentTypeConfig` using override or tenant's `agent_type`.
6. Unknown agent type returns `EngineError::AgentTypeNotFound`.
7. Calls `TaskManager::create_task(...)` which creates the task and transitions to Running.
8. Extracts `broadcast::Sender<Value>` from TaskManager via `get_event_sender`.
9. `ResourceBudget` created from tenant's `BudgetPoolConfig`.
10. Fresh `MemoryFs` per task, `/workspace/task.md` seeded with description, catalog skills snapshotted into read-only `/skills/<name>/SKILL.md` and `/var/skills/<name>.md`.
11. VFS host mounts processed from tenant's `vfs_root`.
12. `ServiceFs` scoped to tenant's integration grants (using the `IntegrationRegistry`).
13. `ProcFs` wraps with task_id as agent_id.
14. `HookPipeline` built from `SimulacraConfig.hooks` (global hooks).
15. `AgentCell` constructed with full composition.
16. Integration registry wired for credential injection, scoped to tenant.
17. `ToolRegistry` populated: builtins, the single S017 `Skill` tool when model-visible catalog skills remain, MCP, WASM (feature-gated), Python (feature-gated).
18. `EngineActivitySink` created with cloned broadcast sender. No TaskManager reference.
19. `AgentLoopConfig` built from agent type's system prompt, model, max_turns, capabilities.
20. Provider constructed via shared `build_provider(model)`.
21. `AgentLoop` constructed with: `AgentLoopConfig`, provider, `ToolRegistry`, `NoopContextStrategy`, journal (InMemoryJournalStorage), budget, hook pipeline, activity sink.
22. `CancellationToken` created and stored in TaskManager.
23. `tokio::spawn` fires with panic-safe wrapper. `spawn_task` returns `Ok(TaskHandle)` immediately.
24. If agent construction (steps 8-22) fails, `complete_task(task_id, Failed, Some(error))` is called and the error is returned.

### Background execution
25. Spawned task calls `agent_loop.run(&description).await`.
26. `ActivityEvent` emissions translated by `EngineActivitySink` and sent on broadcast channel.
27. Every emitted event carries a monotonic `seq` number (from EngineActivitySink's AtomicU64).
28. On `Ok(output)`, `ExitReason` mapped to terminal `TaskState` (or `WaitingApproval` for `AwaitingApproval`).
29. On `Err(e)`, task transitions to `Failed` with error message as reason.
30. On panic, task transitions to `Failed` with "agent task panicked" as reason.
31. `complete_task` called with the `reason` parameter, emitting final `task.state_changed` event with reason.

### Event bridging
32. Each `ActivityEvent` variant translated to server event JSON per the mapping table above.
33. `ChildActivity` events are flattened recursively: the inner event is emitted with `child_id` and `child_agent_type` fields added.
34. All events include `task_id` and `seq`.
35. `Token` → `agent.message` with `content` and `role: "assistant"`.
36. `ThinkStart` → `agent.thinking` with `state: "started"`.
37. `ThinkDelta` → `agent.thinking` with `content`.
38. `ThinkEnd` → `agent.thinking` with `state: "ended"`, `duration_ms`, `tokens`.
39. `ToolStart` → `tool.called` with `tool_call_id`, `tool_name`, `arguments`.
40. `ToolApprovalRequired` → `tool.approval_required` with `tool_call_id`, `tool_name`, `arguments`, `reason`.
41. `ToolCallDelta` → `tool.call_delta` with `index`, optional `tool_call_id`, optional `tool_name`, and `arguments_delta`.
42. `ToolOutput` → `tool.output` with `tool_call_id`, `line`.
43. `InputRequired` → `input.required` with `prompt` and optional `schema`.
44. `ToolFinish` → `tool.result` with `tool_call_id`, `tool_name`, `is_error`, `duration_ms`.
45. `ChildSpawned` → `agent.child_spawned` with `child_id`, `agent_type`, `child_task`.
46. `ChildFinished` → `agent.child_finished` with `child_id`, `agent_type`, `exit_reason`, `duration_ms`.
47. `TurnComplete` → `agent.turn_complete`.

### TaskManager emit_event
48. Acquires lock, looks up task, increments seq, calls `event_tx.send(event)` with seq embedded.
49. Returns `NotFound` for nonexistent task.
50. Silently drops if no subscribers.
51. Every event (including activity events, not just state transitions) carries a monotonic seq.

### Cancellation
52. `task.cancel` command triggers `CancellationToken::signal()` on the task's token.
53. The agent loop checks `cancellation.is_cancelled()` each turn and exits with `ExitReason::Cancelled`.
54. The background task then calls `complete_task(task_id, Cancelled, None)`.
55. If no cancellation token is stored (shouldn't happen), cancel_task falls back to existing behavior (immediate state transition).

### Input and approval
56. `input.response` sends on the task's `input_tx` channel (if present).
57. `approval.respond` sends on the task's `approval_tx` channel (if present).
58. HITL-enabled AgentLoops consume these channels and resume the same live task.
59. `enable_human_input` task metadata exposes `request_input`; `require_tool_approval` task metadata pauses tool calls before execution.

### Budget warnings
58. Budget warning events emitted at 80% and 95% token/cost thresholds.
59. Emitted as `budget.warning` events on the task's broadcast channel.

### Isolation
60. Each task: own VFS instance (fresh MemoryFs).
61. Each task: own AgentCell.
62. Each task: own ResourceBudget.
63. Each task: own Journal (InMemoryJournalStorage).
64. Each task: own ToolRegistry.
65. Each task: own CancellationToken.
66. `SimulacraConfig` and `IntegrationRegistry` shared read-only across all tasks.

### Server startup
67. `start_server` accepts `ServerConfig`, `SimulacraConfig`, `AuthProvider`, `TenantResolver`.
68. `IntegrationRegistry` constructed from `SimulacraConfig`.
69. `SimulacraEngine::new(simulacra_config, integration_registry)` called — returns error on validation failure.
70. `AppState` stores `Arc<SimulacraEngine>` (real engine, not empty stub).
71. REST/WS `task.create` handlers call `engine.spawn_task(...)`.

### Error handling
72. `SimulacraEngine::new` returns `Err` on config validation failures.
73. `spawn_task` agent construction failures: task transitions to `Failed`, error returned to caller.
74. Background execution errors → `Failed` with error message.
75. Background panics → `Failed` with "agent task panicked".
76. Provider env var missing → `EngineError::MissingEnvVar`.

## Assertions

### Engine construction
- [x] `SimulacraEngine::new` returns `Ok` with valid config and no integration registry.
- [x] `SimulacraEngine::new` returns `Ok` with valid config and integration registry.
- [x] `SimulacraEngine::new` returns `Err` when a tenant references a nonexistent agent type.

### Task spawning
- [x] `spawn_task` returns `Err(AgentTypeNotFound)` for unknown agent type.
- [x] `spawn_task` returns `Ok(TaskHandle)` immediately (non-blocking after spawn).
- [x] Task is in `Running` state in the returned TaskHandle.
- [x] Constructs fresh `MemoryFs` per task.
- [x] Seeds `/workspace/task.md` with description.
- [x] Validates catalog skill names as single VFS path segments, then snapshots catalog-backed skills into read-only canonical `/skills/<name>/SKILL.md` files for S017 discovery.
- [x] Constructs `ServiceFs` scoped to tenant integrations.
- [x] Constructs `ProcFs` with task_id as agent_id.
- [x] Constructs `AgentCell` with correct capabilities from `AgentTypeConfig`.
- [x] Creates `ResourceBudget` from tenant config's `BudgetPoolConfig`.
- [x] Builds `HookPipeline` from `SimulacraConfig.hooks` (global hooks, not per-tenant).
- [x] Registers builtins in `ToolRegistry`.
- [x] Discovers and filters skills, then registers exactly one model-visible `Skill` tool when any catalog skill remains model-invocable.
- [x] Registers WASM tools when feature-gated.
- [x] Registers Python tools when feature-gated.
- [x] Constructs provider via shared `build_provider`.
- [x] Missing `ANTHROPIC_API_KEY` for claude model returns `EngineError::MissingEnvVar`.
- [x] Missing `OPENAI_API_KEY` for OpenAI model returns `EngineError::MissingEnvVar`.
- [x] Two concurrent `spawn_task` calls produce independent environments.
- [x] Agent construction failure after task creation transitions task to `Failed`.

### Event bridging
- [x] `Token` → `agent.message` with correct `task_id` and `seq`.
- [x] `ToolStart` → `tool.called` with `tool_call_id`, `tool_name`, `arguments`, `seq`.
- [x] `ToolApprovalRequired` → `tool.approval_required` with `tool_call_id`, `tool_name`, `arguments`, `reason`, and `seq`. **Tested by `hitl_activity_events_translate_to_server_events_and_waiting_states`.**
- [x] `ToolCallDelta` → `tool.call_delta` with `index`, optional `tool_call_id`, optional `tool_name`, `arguments_delta`, and `seq`. **Tested by `tool_call_delta_events_translate_to_tool_call_delta_with_optional_metadata_and_seq`.**
- [x] `ToolOutput` → `tool.output` with `tool_call_id`, `line`, `seq`.
- [x] `InputRequired` → `input.required` with `prompt`, optional `schema`, and `seq`. **Tested by `hitl_activity_events_translate_to_server_events_and_waiting_states`.**
- [x] `ToolFinish` → `tool.result` with `duration_ms`, `is_error`, `seq`.
- [x] `ThinkStart`/`ThinkDelta`/`ThinkEnd` → `agent.thinking` events with `seq`.
- [x] `ChildSpawned` → `agent.child_spawned` with `seq`.
- [x] `ChildFinished` → `agent.child_finished` with `seq`.
- [x] `TurnComplete` → `agent.turn_complete` with `seq`.
- [x] `ChildActivity` flattened: inner event emitted with `child_id` field added.
- [x] Nested `ChildActivity` (grandchild) flattened correctly with innermost child_id.
- [x] All events include `task_id` and monotonic `seq`.
- [x] Events received by TaskManager broadcast subscribers.
- [x] `EngineActivitySink::emit` avoids TaskManager lock acquisition for ordinary activity events; HITL wait events intentionally call TaskManager to transition waiting state. **Tested by `hitl_activity_events_translate_to_server_events_and_waiting_states`.**

### Agent completion
- [x] `Complete` → `Completed` with no reason.
- [x] `MaxTurns` → `Completed` with `reason: "max_turns"`.
- [x] `BudgetExhausted` → `Killed` with `reason: "budget_exhausted"`.
- [x] `Cancelled` → `Cancelled` with no reason.
- [x] `GuardrailTripped(msg)` → `Killed` with reason containing msg.
- [x] `PolicyKill { hook, reason }` → `Killed` with reason containing hook and reason.
- [x] `Error(msg)` → `Failed` with reason containing msg.
- [x] `AwaitingApproval` → `WaitingApproval` (non-terminal).
- [x] `run()` returning `Err` → `Failed` with error message as reason.
- [x] Agent task panic → `Failed` with "agent task panicked" as reason.
- [x] Terminal transition emits final `task.state_changed` with `reason` field.

### Cancellation
- [x] `task.cancel` signals the `CancellationToken` for the task.
- [x] Agent loop observes cancellation and exits with `Cancelled`.
- [x] Task transitions to `Cancelled` after agent loop exits.

### Budget warnings
- [x] `budget.warning` emitted at 80% token threshold.
- [x] `budget.warning` emitted at 95% token threshold.

### TaskManager emit_event
- [x] Sends JSON on task broadcast channel.
- [x] Returns `NotFound` for nonexistent task.
- [x] Increments seq on every event (not just state transitions).
- [x] Non-blocking (broadcast send).

### TaskManager get_event_sender
- [x] Returns cloned `broadcast::Sender` for existing task.
- [x] Returns `NotFound` for nonexistent task.

### Provider resolution
- [x] `"claude-sonnet-4-6"` → `ProviderKind::Anthropic`.
- [x] `"gpt-4o"` → `ProviderKind::OpenAI`.
- [x] `"ollama:llama3"` → `ProviderKind::Ollama`.
- [x] Missing env var → `EngineError::MissingEnvVar`.

### Integration test (end-to-end)
- [x] REST POST creates task and spawns agent (real `SimulacraEngine`, not stub).
- [x] SSE stream receives `tool.called` and `tool.result` with `seq` during execution.
- [x] SSE stream receives `task.state_changed` to `completed` with `seq`.
- [x] SSE stream closes after terminal state.
- [x] WebSocket `task.create` spawns agent and events received with `seq`.
- [x] Late SSE subscriber receives events from subscription point (not from task start).

### Isolation
- [x] Two concurrent tasks have independent VFS.
- [x] Budget exhaustion in task A doesn't affect task B.
- [x] Each task has own journal.
- [x] Each task has own CancellationToken.

### Server startup
- [x] `start_server` constructs `SimulacraEngine` from `SimulacraConfig`.
- [x] `AppState` contains real `SimulacraEngine` (not empty stub).
- [x] REST/WS handlers use `engine.spawn_task()`.

## Observability (see S010)

- [x] `simulacra_engine_spawn_task` span with task_id, tenant, agent_type, model.
- [x] `simulacra_engine_agent_run` span with task_id, exit_reason.
- [x] `simulacra.engine.active_agents` gauge.
- [x] `simulacra.engine.tasks_spawned` counter with tenant, agent_type labels.
- [x] `simulacra.engine.tasks_completed` counter with tenant, agent_type, terminal_state labels.
- [x] `simulacra.engine.spawn_duration` histogram.
- [x] `simulacra.engine.agent_duration` histogram.
- [x] `tracing::info!` on spawn with task_id, tenant, agent_type, model.
- [x] `tracing::info!` on completion with task_id, exit_reason, duration, reason.
- [x] `tracing::warn!` on agent error with task_id and error.
- [x] `tracing::error!` on spawn failure with task_id and error.
- [x] `tracing::error!` on agent panic with task_id.
