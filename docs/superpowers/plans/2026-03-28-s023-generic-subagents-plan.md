# S023 Generic Sub-Agent Spawning — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend `spawn_agent` to support generic sub-agents where the parent provides a system prompt and optional tier at runtime, without pre-configured agent types.

**Architecture:** Extend `SpawnConfig` with optional `system_prompt` and `tier` fields. `AgentTaskFactory::create_task()` branches: if `agent_type` present → existing config lookup; if absent → use inline system_prompt + tier-resolved model + parent-inherited capabilities. Generic agents are leaf workers (no `SpawnAgentTool` registered). Tiers map to models via `[tiers]` config section.

**Tech Stack:** Rust, simulacra-runtime, simulacra-config, serde

**Spec:** `docs/superpowers/specs/2026-03-28-generic-subagents-design.md`

**Key references:**
- `crates/simulacra-runtime/src/spawn_tool.rs` — SpawnAgentTool definition, call(), AgentTaskFactory
- `crates/simulacra-runtime/src/lib.rs:191-203` — SpawnConfig struct
- `crates/simulacra-config/src/lib.rs` — SimulacraConfig, AgentTypeConfig
- `crates/simulacra-runtime/tests/s018_subagent_red.rs` — existing spawn tests
- `simulacra.toml` — project config

---

## File Structure

### Modified

| File | Change |
|------|--------|
| `crates/simulacra-config/src/lib.rs` | Add `tiers: HashMap<String, String>` to `SimulacraConfig` |
| `crates/simulacra-runtime/src/lib.rs` | Extend `SpawnConfig` with `system_prompt: Option<String>`, `tier: Option<String>`; change `agent_type` from `String` to `Option<String>` |
| `crates/simulacra-runtime/src/spawn_tool.rs` | Update tool schema, validation, call(), and AgentTaskFactory::create_task() |
| `crates/simulacra-runtime/tests/s018_subagent_red.rs` | Add generic spawn tests |
| `specs/S018-interactive-subagents.md` | Reference S023 for generic spawning |
| `specs/SPECS.md` | Add S023 entry |

---

### Task 1: Add `[tiers]` config to SimulacraConfig

**Files:**
- Modify: `crates/simulacra-config/src/lib.rs`

- [ ] **Step 1: Write tests for tier config parsing**

Add to the test module in `crates/simulacra-config/src/lib.rs`:
- `tiers_section_parsed` — TOML with `[tiers]` section, verify HashMap entries
- `tiers_section_absent_defaults_to_empty` — TOML without `[tiers]`, verify empty HashMap
- `tiers_with_custom_names` — non-standard tier names (e.g., `"turbo"`) parse correctly

- [ ] **Step 2: Add `tiers` field to SimulacraConfig**

```rust
pub struct SimulacraConfig {
    pub project: ProjectConfig,
    pub agent_types: HashMap<String, AgentTypeConfig>,
    #[serde(default)]
    pub mcp: Option<McpConfig>,
    #[serde(default)]
    pub task: Option<TaskConfig>,
    #[serde(default)]
    pub vfs: VfsConfig,
    #[serde(default)]
    pub tiers: HashMap<String, String>,  // NEW
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p simulacra-config`

- [ ] **Step 4: Commit**

```bash
git add crates/simulacra-config/
git commit -m "feat(config): add [tiers] config section for model tier mapping [S023]"
```

---

### Task 2: Extend SpawnConfig with generic spawn fields

**Files:**
- Modify: `crates/simulacra-runtime/src/lib.rs`

- [ ] **Step 1: Update SpawnConfig struct**

Change `agent_type` from `String` to `Option<String>`, add `system_prompt` and `tier`:

```rust
pub struct SpawnConfig {
    pub agent_id: AgentId,
    pub parent_id: AgentId,
    pub capability: Option<CapabilityToken>,
    pub budget: ResourceBudget,
    pub restart_strategy: RestartStrategy,
    pub agent_type: Option<String>,          // was String
    pub task: String,
    pub system_prompt: Option<String>,       // NEW
    pub tier: Option<String>,               // NEW
}
```

- [ ] **Step 2: Fix all compilation errors from `agent_type` type change**

Search for all uses of `spawn_config.agent_type` across the codebase. Key locations:
- `spawn_tool.rs` — `SpawnConfig` construction (line ~302)
- `spawn_tool.rs` — `AgentTaskFactory::create_task()` — config lookup (line ~424)
- `spawn_tool.rs` — `ForwardingActivitySink::new()` (line ~511)
- `lib.rs` — supervisor journal entries
- `tests/s018_subagent_red.rs` — test SpawnConfig construction

For each, wrap in `Some(...)` where a configured type is used, or handle the `None` case.

- [ ] **Step 3: Build and verify**

Run: `cargo build --workspace`
Expected: compiles (existing behavior preserved via `Some(agent_type)`)

- [ ] **Step 4: Commit**

```bash
git add crates/simulacra-runtime/
git commit -m "refactor(runtime): make SpawnConfig.agent_type optional, add system_prompt and tier fields [S023]"
```

---

### Task 3: Update SpawnAgentTool schema and validation

**Files:**
- Modify: `crates/simulacra-runtime/src/spawn_tool.rs`

- [ ] **Step 1: Update tool definition schema**

In `SpawnAgentTool::definition()`, change the JSON schema:
- `agent_type` moved out of `required` array (now optional)
- Add `system_prompt`: `{ "type": "string", "description": "System prompt for generic sub-agent (max 8KB). Required when agent_type is omitted." }`
- Add `tier`: `{ "type": "string", "enum": ["reasoning", "balanced", "fast"], "description": "Model capability tier. Defaults to parent's tier." }`
- `required` becomes `["task", "budget"]` (both always required)

- [ ] **Step 2: Update `call()` validation logic**

Replace the current validation (lines ~240-258) with:

```rust
let agent_type = arguments.get("agent_type").and_then(|v| v.as_str()).map(String::from);
let system_prompt = arguments.get("system_prompt").and_then(|v| v.as_str()).map(String::from);
let tier = arguments.get("tier").and_then(|v| v.as_str()).map(String::from);

// Validate: one of agent_type or system_prompt, not both, not neither
match (&agent_type, &system_prompt) {
    (Some(_), Some(_)) => return Err(ToolError::InvalidArguments(
        "provide agent_type or system_prompt, not both".into()
    )),
    (None, None) => return Err(ToolError::InvalidArguments(
        "either agent_type or system_prompt is required".into()
    )),
    _ => {}
}

// Validate agent_type against can_spawn (only for configured mode)
if let Some(ref at) = agent_type {
    if !self.can_spawn.contains(at) {
        return Err(ToolError::ExecutionFailed(
            format!("agent_type '{}' is not in can_spawn config", at)
        ));
    }
}

// Validate system_prompt size (8KB = 8192 bytes)
if let Some(ref sp) = system_prompt {
    if sp.len() > 8192 {
        return Err(ToolError::ExecutionFailed(
            format!("system_prompt exceeds 8KB limit ({} bytes)", sp.len())
        ));
    }
}
```

- [ ] **Step 3: Update SpawnConfig construction in `call()`**

```rust
let config = SpawnConfig {
    agent_id: AgentId(child_id.clone()),
    parent_id: self.parent_id.clone(),
    capability,
    budget: ResourceBudget::new(max_tokens, max_turns, max_cost, max_sub_agents),
    restart_strategy: crate::RestartStrategy::LetCrash,
    agent_type,            // now Option<String>
    task: task.clone(),
    system_prompt,         // NEW
    tier,                  // NEW
};
```

- [ ] **Step 4: Build and verify existing tests still pass**

Run: `cargo build -p simulacra-runtime && cargo test -p simulacra-runtime`

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-runtime/
git commit -m "feat(runtime): update spawn_agent schema — agent_type optional, add system_prompt and tier [S023]"
```

---

### Task 4: Update AgentTaskFactory for generic mode

**Files:**
- Modify: `crates/simulacra-runtime/src/spawn_tool.rs`

- [ ] **Step 1: Add tier resolution helper**

Add a function near the top of `spawn_tool.rs`:

```rust
fn resolve_tier_model(
    tier: Option<&str>,
    tiers_config: &HashMap<String, String>,
    parent_model: &str,
) -> String {
    match tier {
        Some(t) => tiers_config.get(t).cloned().unwrap_or_else(|| parent_model.to_string()),
        None => parent_model.to_string(),
    }
}
```

- [ ] **Step 2: Add `parent_model` and `tiers` to AgentTaskFactory**

`AgentTaskFactory` needs to know the parent's model (for tier inheritance) and the tiers config. Add fields:

```rust
pub struct AgentTaskFactory {
    pub config: SimulacraConfig,
    pub provider_kind: ProviderKind,
    pub vfs: Arc<dyn VirtualFs>,
    pub journal: Arc<dyn JournalStorage>,
    pub activity_sink: Arc<dyn ActivitySink>,
    pub parent_capability: CapabilityToken,
    pub supervisor_sender: Option<mpsc::Sender<SupervisorMessage>>,
    pub parent_model: String,  // NEW — for tier inheritance
}
```

Update all `AgentTaskFactory` construction sites:
- `crates/simulacra-cli/src/lib.rs` — where `AgentTaskFactory` is created in the interactive/headless boot path. Add `parent_model: boot.model.clone()`.
- `crates/simulacra-runtime/tests/s018_subagent_red.rs` — test helpers that construct `AgentTaskFactory`. Add `parent_model` field.

- [ ] **Step 3: Branch `create_task()` for configured vs generic mode**

Replace the current `create_task()` body (lines ~435-530) with branching logic:

```rust
// Configured mode: agent_type is Some
if let Some(ref agent_type_name) = spawn_config.agent_type {
    let agent_type_config = self.config.agent_types.get(agent_type_name).cloned()
        .ok_or_else(|| RuntimeError::Session(format!("unknown agent_type: {}", agent_type_name)))?;

    // ... existing config-lookup path (model, system_prompt, three-way capability intersection)
    // ... register SpawnAgentTool if child_can_spawn
}
// Generic mode: agent_type is None, system_prompt is Some
else if let Some(ref system_prompt) = spawn_config.system_prompt {
    let model = resolve_tier_model(
        spawn_config.tier.as_deref(),
        &self.config.tiers,
        &self.parent_model,
    );

    // Two-way capability intersection: parent ∩ override (no config layer)
    let effective_capability = match spawn_config.capability {
        Some(ref override_cap) => parent_capability.intersect(override_cap),
        None => parent_capability.clone(),
    };

    // ... create provider, AgentLoopConfig, AgentLoop
    // ... NO SpawnAgentTool registered (leaf worker)
}
else {
    return Box::pin(async { Err(RuntimeError::Session("invalid spawn config".into())) });
}
```

- [ ] **Step 4: Ensure generic child has no SpawnAgentTool**

In the generic branch, after `register_builtins()`, do NOT register `SpawnAgentTool`. The child only has file/shell/js tools.

- [ ] **Step 5: Update ForwardingActivitySink for generic agents**

When creating the `ForwardingActivitySink`, use `"generic"` as the agent_type string when `spawn_config.agent_type` is `None`:

```rust
let activity_type = spawn_config.agent_type.clone().unwrap_or_else(|| "generic".to_string());
ForwardingActivitySink::new(
    spawn_config.agent_id.0.clone(),
    activity_type,
    parent_sink,
)
```

- [ ] **Step 6: Build and test**

Run: `cargo build --workspace && cargo test -p simulacra-runtime`

- [ ] **Step 7: Commit**

```bash
git add crates/simulacra-runtime/ crates/simulacra-cli/
git commit -m "feat(runtime): implement generic sub-agent creation in AgentTaskFactory [S023]"
```

---

### Task 5: Write tests for generic spawning

**Files:**
- Modify: `crates/simulacra-runtime/tests/s018_subagent_red.rs`

- [ ] **Step 1: Write generic spawn validation tests**

Add tests using the existing test infrastructure (FakeProvider, RecordingTaskFactory, etc.):

- `generic_spawn_with_system_prompt_creates_child` — spawn with `system_prompt` and no `agent_type`, verify child runs with the provided prompt
- `generic_spawn_with_both_agent_type_and_system_prompt_errors` — verify tool error
- `generic_spawn_with_neither_agent_type_nor_system_prompt_errors` — verify tool error
- `generic_spawn_system_prompt_exceeds_8kb_errors` — 9KB prompt → tool error
- `generic_spawn_inherits_parent_capabilities` — verify child has parent's capability token
- `generic_spawn_with_capability_override_intersects_parent` — verify two-way intersection
- `generic_spawn_cannot_spawn_children` — generic child's tool registry has no spawn_agent
- `generic_spawn_with_tier_uses_tier_model` — verify model from `[tiers]` config
- `generic_spawn_without_tier_inherits_parent_model` — verify parent model inheritance
- `generic_spawn_consumes_parent_budget` — verify `used_sub_agents` incremented
- `configured_spawn_still_works` — existing configured path unchanged (regression test)

- [ ] **Step 2: Run tests**

Run: `cargo test -p simulacra-runtime -- generic_spawn`
Expected: all pass

- [ ] **Step 3: Commit**

```bash
git add crates/simulacra-runtime/
git commit -m "test(runtime): add generic sub-agent spawning tests [S023]"
```

---

### Task 6: Observability and tier validation

**Files:**
- Modify: `crates/simulacra-runtime/src/spawn_tool.rs`
- Modify: `crates/simulacra-runtime/src/lib.rs`

- [ ] **Step 1: Add `simulacra.agent.spawn_mode` to create_agent span**

In `AgentTaskFactory::create_task()`, add the spawn mode attribute to the `create_agent` span:

```rust
let spawn_mode = if spawn_config.agent_type.is_some() { "configured" } else { "generic" };
let tier_label = spawn_config.tier.as_deref().unwrap_or("inherited");

let agent_span = tracing::info_span!(
    "create_agent",
    // ... existing attributes ...
    "simulacra.agent.spawn_mode" = spawn_mode,
    "simulacra.agent.tier" = tier_label,
);
```

- [ ] **Step 2: Include system_prompt in SubAgentSpawned journal entry**

In the supervisor's spawn handling (in `lib.rs`), when writing the `SubAgentSpawned` journal entry, include the system prompt text for audit. The `agent_type` field should be `"generic"` when `spawn_config.agent_type` is `None`.

- [ ] **Step 3: Add tier validation in `call()`**

In `SpawnAgentTool::call()`, after parsing `tier`, validate against the tiers config. The tool needs access to the tiers map — add it as a field on `SpawnAgentTool`:

```rust
pub struct SpawnAgentTool {
    pub sender: mpsc::Sender<SupervisorMessage>,
    pub can_spawn: Vec<String>,
    pub activity_sink: Arc<dyn ActivitySink>,
    pub parent_id: AgentId,
    pub tiers: HashMap<String, String>,  // NEW — from SimulacraConfig.tiers
}
```

Validation:
```rust
if let Some(ref t) = tier {
    if !self.tiers.is_empty() && !self.tiers.contains_key(t.as_str()) {
        let valid: Vec<_> = self.tiers.keys().collect();
        return Err(ToolError::ExecutionFailed(
            format!("unknown tier '{}'. Valid tiers: {:?}", t, valid)
        ));
    }
}
```

Update all `SpawnAgentTool` construction sites to pass `tiers`.

- [ ] **Step 4: Write tier validation test**

- `generic_spawn_with_unknown_tier_errors` — `tier: "turbo"` returns tool error listing valid tiers

- [ ] **Step 5: Build, test, clippy, fmt**

Run: `cargo build --workspace && cargo test -p simulacra-runtime && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-runtime/ crates/simulacra-cli/
git commit -m "feat(runtime): add spawn_mode/tier observability, tier validation [S023]"
```

---

### Task 7: Update specs and docs

**Files:**
- Modify: `specs/S018-interactive-subagents.md`
- Modify: `specs/SPECS.md`
- Modify: `simulacra.toml`

- [ ] **Step 1: Add S023 reference to S018**

Add a section to S018 noting that generic spawning is specified in S023.

- [ ] **Step 2: Add S023 to SPECS.md**

Add row: `| specs/S023-generic-subagents.md | Active | Generic sub-agent spawning with runtime-defined system prompts and tiers |`

- [ ] **Step 3: Add example tiers to simulacra.toml**

```toml
[tiers]
reasoning = "claude-opus-4-6"
balanced = "claude-sonnet-4-6"
fast = "claude-haiku-4-5-20251001"
```

- [ ] **Step 4: Commit**

```bash
git add specs/ simulacra.toml
git commit -m "docs: update S018 reference, add S023 to SPECS.md, add tiers to simulacra.toml [S023]"
```
