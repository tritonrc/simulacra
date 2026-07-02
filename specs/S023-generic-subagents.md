# S023 — Generic Sub-Agent Spawning

**Status:** Active
**Crates involved:** `simulacra-runtime`, `simulacra-config`

## Dependencies

- **S018** — Interactive sub-agent spawning (spawn_agent tool, supervisor, capability attenuation)
- **S004** — Capability tokens (intersection semantics)
- **S006** — Resource budgets (parent-child delegation)

## Scope

Extend `spawn_agent` to support generic sub-agents: parent provides system prompt + optional tier at runtime, without pre-configured agent types. Generic agents are leaf workers (cannot spawn children).

Full design: `docs/superpowers/specs/2026-03-28-generic-subagents-design.md`

## Clarifications

- **`max_sub_agents: 0` means unlimited**, consistent with the `0 = unlimited` convention
  established by S006/S018 for all budget fields (`max_tokens`, `max_turns`, `max_cost`,
  `max_sub_agents`). Operators disable spawning by not registering the `spawn_agent` tool
  (i.e., by leaving `spawn_types` empty in the capability token), not by setting
  `max_sub_agents` to zero.

## Behavior

### Tool schema

- [x] `spawn_agent` tool schema makes `agent_type` optional.
- [x] `spawn_agent` tool schema accepts optional `system_prompt` string.
- [x] `spawn_agent` tool schema accepts optional `tier` string.
- [x] `budget` remains required in both configured and generic modes.
- [x] `capabilities` override remains accepted in both modes.

### Validation — mutually-exclusive mode selection

- [x] `spawn_agent` with both `agent_type` and `system_prompt` returns a tool error.
- [x] `spawn_agent` with neither `agent_type` nor `system_prompt` returns a tool error.
- [x] `spawn_agent` with `agent_type = ""` (empty string) returns a tool error rather than silently entering generic mode.
- [x] Empty `system_prompt` (when `agent_type` is absent) falls back to a default prompt rather than erroring.

### Validation — size and enum constraints

- [x] `system_prompt` exceeding 8,192 bytes returns a tool error with a size-limit message.
- [x] `tier` not present in `[tiers]` config returns a tool error listing valid tier names.
- [x] `agent_type` not in the parent's `can_spawn` list returns a tool error (existing S018 behaviour preserved).

### Generic agent creation

- [x] Generic agent uses the caller-supplied `system_prompt` as its system message.
- [x] Generic agent with a known `tier` resolves to the model from `[tiers]` config.
- [x] Generic agent with no `tier` inherits the parent's model.
- [x] Generic agent with no `capabilities` override inherits the parent's full capability token.
- [x] Generic agent with a `capabilities` override gets `parent ∩ override` (two-way intersection).
- [x] Generic agent's `can_spawn` is always empty — it cannot spawn children.
- [x] Generic agent's tool registry includes all builtin tools (file_read, file_write, apply_patch, shell_exec, js_exec, list_dir) and excludes `spawn_agent`. **Tested in `generic_spawn_tool_registry_includes_all_builtins_and_excludes_spawn_agent`.**

### Tier configuration

- [x] `[tiers]` section in `simulacra.toml` deserializes into `SimulacraConfig.tiers: HashMap<String, String>`.
- [x] Missing `[tiers]` section deserializes to an empty map (all lookups fall through to parent model).
- [x] Custom tier names are accepted — any string key in `[tiers]` is valid (not restricted to reasoning/balanced/fast).
- [x] Parent tier is determined by reverse-lookup: first `[tiers]` entry whose value matches the parent's model wins; if no match, parent tier is `"balanced"`. **`SimulacraConfig.tiers` preserves TOML order via `TierMap`; tested in `generic_spawn_without_tier_reverse_looks_up_parent_model_for_resolved_tier` and `generic_create_agent_span_labels_missing_tier_as_balanced_fallback`.**

### Budget and supervisor

- [x] Generic spawn consumes from the parent's `max_sub_agents` budget.
- [x] Generic spawn flows through the same supervisor validation as configured spawns (budget headroom, capability check, journal entry).
- [x] Child budget is rolled up to the parent after the generic child completes.
- [x] `max_sub_agents: 0` inherits S006's "unlimited" semantics — kill-switch is the absence of `spawn_agent` in the tool registry, not a zero budget. **Tested in `generic_spawn_parent_max_sub_agents_zero_remains_unlimited`.**

### What does not change (S018 regression gates)

- [x] Configured-mode spawn (`agent_type` present) follows the existing S018 path with no behavioural change.
- [x] `capabilities` override JSON parses into `SpawnConfig` identically for both configured and generic modes.
- [x] Supervisor actor loop message flow is unchanged.
- [x] `ChildSpawned` / `ChildFinished` activity events continue to fire for generic children.

## Observability (see S010)

- [x] `SubAgentSpawned` journal entry records `agent_type: "generic"` and includes the full `system_prompt` text for audit. **Implemented as `JournalEntryKind::SubAgentSpawned { system_prompt }`; tested in `generic_subagent_spawned_journal_records_full_system_prompt_for_audit` and `generic_spawn_aborts_when_subagent_spawned_journal_append_fails`.**
- [x] `create_agent` span carries a `simulacra.agent.spawn_mode` attribute — `"configured"` or `"generic"`. **Tested in `generic_create_agent_span_records_generic_spawn_mode_and_explicit_tier`.**
- [x] `create_agent` span carries a `simulacra.agent.tier` attribute recording the resolved tier name. **Tested for explicit, inherited reverse-lookup, and fallback labels.**
- [x] Generic child spans nest under the parent trace (S018 parent-child relationship preserved). **Tested in `generic_child_invoke_agent_span_nests_under_parent_trace`.**
