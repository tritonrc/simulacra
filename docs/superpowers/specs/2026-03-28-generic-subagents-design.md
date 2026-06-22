# S023 — Generic Sub-Agent Spawning

**Status:** Active
**Crates involved:** `simulacra-runtime`, `simulacra-config`

## Dependencies

- **S018** — Interactive sub-agent spawning (existing spawn_agent tool, supervisor, capability attenuation)
- **S004** — Capability tokens (intersection semantics, attenuation invariant)
- **S006** — Resource budgets (parent-child budget delegation)

## Scope

Extend `spawn_agent` to support **generic** sub-agents: the parent agent provides a system prompt and optional tier at runtime, without requiring a pre-configured agent type in `simulacra.toml`.

**In scope:**
- Make `agent_type` optional on `spawn_agent`
- Add `system_prompt` and `tier` parameters for inline agent definition
- Add `[tiers]` config section mapping tier names to models
- Generic agents are leaf workers (cannot spawn children)

**Out of scope:**
- Recursive generic spawning (generic agents spawning generic agents)
- Runtime tool registry customization per generic agent (all get the same builtins)
- Agent personality/traits system (future spec — RPG-like character sheets)
- Dynamic capability grants (capabilities can only be attenuated, never widened)

## Context

Today, every sub-agent must be pre-declared as an `[agent_types.X]` entry in `simulacra.toml` with a name, model, system prompt, and capabilities. The parent agent references child types by name: `spawn_agent(agent_type: "researcher", ...)`.

This is limiting. A coding agent that wants to delegate a security review, a performance analysis, and a documentation pass has to know at config time that these three roles exist. In practice, the LLM is better at deciding what specialist it needs at runtime than the operator is at predicting every possible role ahead of time.

Generic spawning lets the parent say: "I need a sub-agent with this personality and this task" — the same way a manager delegates to a team member by explaining what they need done, not by looking up a job title in an HR system.

The security model is preserved: generic agents inherit capabilities from the parent (attenuated, never widened), are bounded by allocated budget, and cannot spawn their own children. The operator controls the ceiling; the agent controls the specialization within that ceiling.

## Design

### Updated spawn_agent tool schema

Two modes — configured and generic:

**Configured mode** (existing, unchanged):
```json
{
  "agent_type": "researcher",
  "task": "Find the latest pricing for...",
  "budget": { "max_tokens": 8192, "max_turns": 10, "max_cost": "0.50", "max_sub_agents": 0 }
}
```

**Generic mode** (new):
```json
{
  "task": "Analyze this code for security vulnerabilities",
  "system_prompt": "You are a security auditor. Focus on OWASP top 10...",
  "budget": { "max_tokens": 8192, "max_turns": 10, "max_cost": "0.50", "max_sub_agents": 0 },
  "tier": "reasoning"
}
```

Schema changes to `spawn_agent`:
- `agent_type` becomes **optional** (was required)
- `system_prompt` added as **optional** string (max 8KB)
- `tier` added as **optional** enum: `"reasoning"`, `"balanced"`, `"fast"`
- `capabilities` override remains optional (works in both modes)
- `budget` remains required

Validation:
- If `agent_type` present → config lookup (existing path). `system_prompt` and `tier` are ignored.
- If `agent_type` absent → `system_prompt` is required. `tier` defaults to parent's tier.
- If both `agent_type` AND `system_prompt` present → tool error: "provide agent_type or system_prompt, not both"
- If neither `agent_type` nor `system_prompt` present → tool error: "either agent_type or system_prompt is required"
- If `system_prompt` exceeds 8KB → tool error: "system_prompt exceeds 8KB limit"
- If `tier` is not a recognized tier name → tool error listing valid tiers

### Tier configuration

Tiers map semantic labels to model strings in `simulacra.toml`:

```toml
[tiers]
reasoning = "claude-opus-4-6"
balanced = "claude-sonnet-4-6"
fast = "claude-haiku-4-5-20251001"
```

- If `[tiers]` section is absent, all three tiers default to the parent's model
- Parent's tier is determined by reverse-lookup: find which tier matches the parent's model. If no match, parent is `balanced`
- When a generic agent is spawned with no `tier`, it inherits the parent's tier
- Configured agent types (`agent_type: "researcher"`) ignore the tier system — they use their own `model` field from config
- Operators can remap tiers without any agent code changes (e.g., swap `fast` from Haiku to GPT-5.4-mini)

### Capability model for generic agents

Generic agents inherit capabilities from the parent, attenuated by optional override:

- **No capabilities override** → child gets parent's full capability token
- **With capabilities override** → two-way intersection: `parent ∩ override`
- **can_spawn** → always empty for generic agents (leaf workers)
- Parent with restricted capabilities creates restricted children — attenuation invariant preserved

### SpawnConfig changes

```rust
pub struct SpawnConfig {
    pub agent_type: Option<String>,        // was required String
    pub task: String,
    pub budget: ResourceBudget,
    pub capability: Option<CapabilityToken>,
    pub system_prompt: Option<String>,     // NEW — for generic mode
    pub tier: Option<String>,              // NEW — "reasoning"/"balanced"/"fast"
}
```

### AgentTaskFactory branching

```
if agent_type is Some:
    → existing path: config lookup, model from config, system_prompt from config
    → three-way capability intersection (config ∩ parent ∩ override)
    → can_spawn from agent_type config

if agent_type is None:
    → system_prompt from SpawnConfig.system_prompt
    → model from tier resolution (look up tier in [tiers] config, or inherit parent model)
    → two-way capability intersection (parent ∩ override)
    → can_spawn = vec![] (leaf worker)
    → child SpawnAgentTool has empty can_spawn
```

### SpawnAgentTool::call() validation

- If `agent_type` present: check `can_spawn` allowlist (existing behavior)
- If `agent_type` absent: no allowlist check (generic spawn permitted if parent has budget). Operators who don't want generic spawning remove the tool from the registry or restrict `max_sub_agents` to 0.

## Behavior

### Tool schema

1. `spawn_agent` tool definition includes `system_prompt` (optional string, max 8KB) and `tier` (optional enum) in addition to existing parameters.
2. `agent_type` becomes optional in the tool schema.
3. `budget` remains required in both modes.

### Validation

4. If `agent_type` and `system_prompt` are both present, return tool error.
5. If neither `agent_type` nor `system_prompt` is present, return tool error.
6. If `system_prompt` exceeds 8,192 bytes, return tool error with size limit message.
7. If `tier` is provided but not in the configured `[tiers]` map, return tool error listing valid tiers.
8. If `agent_type` is present, existing validation applies (must be in `can_spawn`, must exist in config).

### Generic agent creation

9. Generic agent's system prompt is the `system_prompt` string from the tool call.
10. Generic agent's model is resolved from `tier`: look up in `[tiers]` config. If `tier` is absent, inherit parent's model.
11. Generic agent's capabilities are `parent ∩ override` (two-way intersection). If no override, inherit parent's full token.
12. Generic agent's `can_spawn` is always empty — cannot spawn children.
13. Generic agent's tool registry includes all builtins (file_read, file_write, file_edit, shell_exec, js_exec, list_dir). `SpawnAgentTool` is NOT registered for generic agents — they cannot see or attempt to spawn children.

### Tier configuration

14. `SimulacraConfig` gains an optional `tiers: HashMap<String, String>` field, deserialized from `[tiers]` in `simulacra.toml`.
15. If `[tiers]` section is absent, all tier lookups return the parent's model.
16. Default tier names: `reasoning`, `balanced`, `fast`. Custom names are allowed — any string key in the map is valid.
17. Parent's tier is determined by reverse-lookup: scan `[tiers]` for a value matching the parent's model. First match wins. If no match, parent tier is `"balanced"`.

### Budget and supervisor

18. Generic spawns go through the same supervisor validation as configured spawns: budget headroom check, `used_sub_agents` increment, journal entry.
19. `max_sub_agents: 0` in the parent's budget prevents all spawning (generic and configured). This is the operator's kill switch.

### What doesn't change

20. Configured agent type spawning (`agent_type` present) follows the existing S018 path with no behavioral changes.
21. Supervisor actor loop message flow is unchanged.
22. Child `AgentLoop::run()` execution is unchanged.
23. Budget rollup from child to parent is unchanged.
24. Activity events (`ChildSpawned`, `ChildFinished`) are unchanged in structure.

## Assertions

### Tool schema

- [ ] `spawn_agent` definition includes optional `system_prompt` and `tier` fields.
- [ ] `agent_type` is optional in the tool schema.
- [ ] Configured mode (with `agent_type`) still works identically to S018.

### Validation

- [ ] `spawn_agent` with both `agent_type` and `system_prompt` returns tool error.
- [ ] `spawn_agent` with neither `agent_type` nor `system_prompt` returns tool error.
- [ ] `system_prompt` exceeding 8KB returns tool error with size message.
- [ ] Unknown `tier` value returns tool error listing valid tiers.
- [ ] `agent_type` not in `can_spawn` returns tool error (existing behavior preserved).

### Generic agent creation

- [ ] Generic agent uses provided `system_prompt` as its system message.
- [ ] Generic agent with `tier: "reasoning"` uses the model mapped in `[tiers]` config.
- [ ] Generic agent with no `tier` inherits parent's model.
- [ ] Generic agent with `capabilities` override gets `parent ∩ override`.
- [ ] Generic agent with no `capabilities` override gets parent's full token.
- [ ] Generic agent does not have `spawn_agent` in its tool registry.
- [ ] Generic agent has access to all builtin tools (file, shell, js).

### Tier configuration

- [ ] `[tiers]` section in `simulacra.toml` is parsed into `SimulacraConfig.tiers`.
- [ ] Missing `[tiers]` section defaults all lookups to parent's model.
- [ ] Parent's tier is determined by reverse-lookup against `[tiers]` values.

### Budget and supervisor

- [ ] Generic spawn consumes from parent's `max_sub_agents` budget.
- [ ] Generic spawn is validated by supervisor (budget headroom, capability attenuation).
- [ ] `max_sub_agents: 0` prevents generic spawning.
- [ ] Child budget is rolled up to parent after completion.

### Observability

- [ ] `SubAgentSpawned` journal entry records `agent_type: "generic"` and includes the system prompt text.
- [ ] `create_agent` span includes `simulacra.agent.spawn_mode` attribute (`"configured"` or `"generic"`).
- [ ] Generic child's spans nest under the parent trace (existing behavior).

## Observability (see S010)

- `create_agent` span gains `simulacra.agent.spawn_mode` attribute: `"configured"` or `"generic"`
- `SubAgentSpawned` journal entry: `agent_type` field is `"generic"` for generic agents, the full `system_prompt` text is stored in the entry for audit
- All existing spawn metrics (`simulacra.agent.turns`, `simulacra.agent.budget.*`) apply to generic children
- Tier selection is recorded on the `create_agent` span as `simulacra.agent.tier`
