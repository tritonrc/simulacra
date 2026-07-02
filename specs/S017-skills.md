# S017 — Skills

**Status:** Active
**Crates involved:** `simulacra-cli`, `simulacra-runtime`, `simulacra-tool`, `simulacra-types`, `simulacra-vfs`

## Dependencies

- **ARCHITECTURE.md** — Golden Rule, capability attenuation, host-side enforcement, OTel conventions
- **S001** — VFS (skill files and resources live in the virtual filesystem)
- **S020** — VFS Host Mounts (host skill roots are mounted into the VFS before discovery)
- **S004** — Capability tokens (skills are gated at the call site and attenuated on child agents)
- **S005** — Journal (tool calls and user-triggered skill loads must remain attributable)
- **S010** — Observability conventions (span/event naming)
- **S011** — Sandbox composition (all side effects still flow through `AgentCell`)
- **S012** — Built-in tools (tool registration, schema conventions, tool result flow)
- **S013** — CLI (bootstrap, config loading, agent-type wiring)
- **S015** — Interactive mode (slash-command handling, approval flow, multi-turn context)
- **S018** — Interactive sub-agents (agent-type config, capability attenuation, child isolation)

## Context

Skills exist to give Simulacra reusable higher-level behaviors without bloating every agent's initial context. A skill is not a new tool and not a new execution surface. It is prompt text plus optional supporting files that teaches the model how to use the tools Simulacra already has.

This spec uses a VFS-native progressive-disclosure model:

1. At startup, Simulacra injects only compact skill metadata (name + description) into the `Skill` tool definition. Full skill bodies are NOT added to the system prompt.
2. When the model invokes `Skill`, Simulacra loads the requested `SKILL.md` body on demand and returns it as the tool result.
3. Supporting files remain in the VFS until the agent explicitly reads or executes them via existing tools such as `file_read`, `list_dir`, `shell_exec`, or `js_exec`.

The Golden Rule still applies. Skills do not bypass capability checks, budgets, journaling, or observability. The skill body is only prompt text. Any real side effect still goes through `AgentCell`.

This spec does NOT define a skill marketplace, remote skill downloads at run time, or a new scripting runtime. It defines discovery, registration, invocation, gating, and context-efficient loading of skill prompts.

## Design

```text
Bootstrap
   |
   +--> discover skill directories
   |      - project VFS: /skills/**/SKILL.md
   |      - configured host skill paths mounted into VFS
   |
   +--> parse frontmatter
   |      - name
   |      - description
   |      - invocation flags
   |      - allowed_tools
   |
   +--> build per-agent skill catalog
          - filter by agent_type.skills
          - filter by capability token
          - keep only name + description in Skill tool definition
          - enforce metadata context budget
                              |
                              v
                     Provider sees one tool:
                     Skill(command = "rust-dev")
                              |
                              v
                 ToolRegistry::call("Skill", {"command": "rust-dev"})
                              |
                              v
                    SkillTool resolves /skills/.../SKILL.md
                              |
                              +--> AgentCell::read_file(skill_path)
                              |
                              +--> parse + strip YAML frontmatter
                              |
                              v
              ToolResult { content: "<markdown body only>", is_error: false }
                              |
                              v
              Model may then read reference files or run scripts explicitly
              with existing tools; those resources are NOT auto-loaded.
```

## Skill Tool Definition

### `Skill`

**Description:** Load the body of a registered skill on demand. The tool definition exposes only a compact catalog of eligible skills (name + description); full skill text is loaded only when the tool is invoked.

**Input schema:**
```json
{
  "type": "object",
  "properties": {
    "command": {
      "type": "string",
      "description": "Skill identifier from SKILL.md frontmatter.name"
    }
  },
  "required": ["command"],
  "additionalProperties": false
}
```

**Output:** The markdown body of `SKILL.md` with YAML frontmatter removed. The result is plain text, not JSON. If the skill is missing, inaccessible, invalid, or not invocable from the current source, returns an error tool result with a descriptive message.

**Delegates to:** `AgentCell::read_file(skill_path)` for `SKILL.md` loading. The tool parses frontmatter itself and returns only the body.

## Skill File Format

A skill is a directory containing a required `SKILL.md` file and optional supporting resources:

```text
my-skill/
├── SKILL.md
├── reference.md
└── scripts/
```

`SKILL.md` MUST begin with YAML frontmatter followed by a markdown body:

```yaml
---
name: rust-dev
description: Use cargo, rustfmt, clippy, and existing project conventions to implement Rust changes safely.
disable_model_invocation: false
user_invocable: true
allowed_tools:
  - file_read
  - shell_exec
---
```

Frontmatter fields:

- `name` — required string. This is the canonical skill identifier used by `Skill(command=...)` and `/skill-name`.
- `description` — required string. Compact summary used for discovery and model selection.
- `disable_model_invocation` — optional boolean, default `false`. When `true`, the skill is excluded from the model-visible catalog and the model cannot invoke it through the `Skill` tool.
- `allow_implicit_invocation` — optional boolean, default `true`. When `false`, the skill is excluded from model-visible discovery metadata and cannot be invoked by model-triggered `Skill` calls, but remains available through `/skill-name` when `user_invocable` and capabilities allow it.
- `user_invocable` — optional boolean, default `true`. When `false`, the skill is not available through interactive `/skill-name` invocation.
- `allowed_tools` — optional array of tool names. While the skill is active in the current turn, these tools are treated as pre-approved by the interactive approval layer. This field never grants new capabilities and never creates new tools.

The markdown body after frontmatter is the skill prompt. It may reference sibling files or scripts by path, but those files are not automatically read into context and scripts are not automatically executed.

The frontmatter `name` is authoritative. Directory names are not the source of truth for invocation. A directory that contains `SKILL.md` but lacks valid frontmatter is not a valid skill.

## Behavior

### Discovery and bootstrap

1. Simulacra discovers skills from two sources at bootstrap:
   - project-local VFS paths under `/skills/**/SKILL.md`;
   - catalog-authored skills snapshotted by `SimulacraEngine` into `/skills/<name>/SKILL.md`;
   - configured host skill paths that are mounted read-only into the VFS before discovery (see S020 for mount semantics).
1a. Project-local `/skills` discovery is rooted at the mounted VFS `/skills` directory. Simulacra recursively scans downward from `/skills` for `SKILL.md` files, so grouped paths such as `/skills/<group>/<dir>/SKILL.md` are valid. It MUST NOT walk upward or search for sibling/ancestor `skills/` directories elsewhere in the VFS or host filesystem.
2. After bootstrap, skill resolution is VFS-first. Both project skills and mounted external skills are addressed through canonical VFS paths.
3. Each discovered `SKILL.md` is parsed once at bootstrap to extract frontmatter metadata and its canonical VFS path. The markdown body is NOT retained in the initial prompt state.
4. The skill registry is keyed by frontmatter `name`, not directory name.
5. Duplicate skill names across discovery sources are a startup error. Simulacra must not pick one implicitly.
6. A discovered directory with missing or invalid `SKILL.md` frontmatter is skipped with a warning unless an agent type explicitly references that skill name; referenced invalid skills are a startup error.
7. Agent type config is extended with `skills = ["rust-dev", "code-review"]`. This is the allow-list of skill names that agent type may expose.
8. If an agent type references a skill name that is not discovered successfully, startup fails with an error naming the agent type and missing skill.

### `Skill` tool registration

9. `ToolRegistry` registers exactly one built-in tool named `Skill` when the current agent has at least one model-invocable skill that survives capability filtering and metadata-budget truncation.
10. Simulacra does NOT register one tool per skill. Skills are not first-class tools.
11. The `Skill` tool definition contains only compact metadata for model-invocable skills: `name` + `description`. Full `SKILL.md` bodies are excluded from the initial tool definition and from the system prompt.
12. The `Skill` tool definition is built from the current agent's effective skill catalog after agent-type config and capability filtering are applied.
13. A skill with `disable_model_invocation: true` is excluded from the model-visible `Skill` tool description even if it is otherwise available to the agent.
13a. A skill with `allow_implicit_invocation: false` is excluded from the model-visible `Skill` tool description and model-triggered `Skill` calls return an error result for that skill.
14. A skill with `user_invocable: false` may still appear in the model-visible `Skill` tool description if model invocation is enabled.
14a. If an agent has no model-invocable skills after filtering, Simulacra does not register the `Skill` tool for that agent. User-triggered skill resolution in interactive mode still works for any remaining `user_invocable` skills.

### Model-triggered invocation

15. When the provider emits `Skill { "command": "<name>" }`, Simulacra resolves `<name>` against the current agent's effective skill catalog.
16. On success, the tool reads the corresponding `SKILL.md` through `AgentCell::read_file`, strips YAML frontmatter, and returns only the markdown body as the tool result.
17. The returned skill body becomes part of the conversation only through that tool result. It is not retroactively added to the system prompt.
18. If the named skill is unknown, not in the agent type's configured skill list, or denied by the capability token, `Skill` returns an error tool result. The agent sees the denial reason.
19. If the named skill has `disable_model_invocation: true`, a model-triggered call returns an error tool result even if the model guessed the name.
20. `Skill` never auto-loads sibling resources, never executes scripts, and never expands referenced files inline. Supporting materials remain on disk until explicitly accessed with existing tools.
21. Multiple skills may be loaded in the same turn. Each `Skill` call resolves and returns one skill body independently.

### User-triggered invocation in interactive mode

22. In interactive mode, `/skill-name` is a reserved slash-command form for user-invocable skills.
23. Slash-command resolution order is:
   - built-in interactive commands from S015;
   - resolved user-invocable skill names;
   - otherwise the existing "unknown command" path from S015.
24. When the user enters `/skill-name <args>`, the interactive host resolves `skill-name`, loads the same skill body that `Skill(command="skill-name")` would return, and injects it into the upcoming turn context before sending the optional trailing `<args>` text to the model.
25. The optional `<args>` string is treated as the user's task or instruction for that skill invocation. If no args are provided, the turn still loads the skill and the model may ask a follow-up question.
26. User-triggered skill loading does not require model approval and does not appear as an LLM-emitted tool call. It is a host-side context injection path using the same resolver and file format as the `Skill` tool.
27. A skill with `user_invocable: false` is not available through `/skill-name`. Direct invocation falls through to the S015 unknown-command behavior.
28. A skill that is blocked by capability policy is not invocable through `/skill-name`, even if it exists on disk.

### Context budget for skill metadata

29. Simulacra derives a skill-metadata budget as a configured percentage of the active model's context window when that context is available; otherwise it uses an 8,000 character fallback budget.
30. Only model-invocable skills in the current agent's effective catalog consume this budget.
31. Metadata entries are considered in the order listed by `agent_type.skills`.
32. Simulacra includes as many `name + description` entries as fit within the metadata budget, truncating oversized descriptions before omitting entire skill entries. Skills that still do not fit are omitted from the model-visible `Skill` tool definition.
33. Omitted skills remain resolvable for user-triggered invocation if they are `user_invocable: true` and otherwise allowed.
34. If one or more model-invocable skills are omitted due to the metadata budget, the `Skill` tool description MUST indicate that the catalog is partial.

### Skill file resolution and on-demand resources

35. Project skills live at `/skills/**/SKILL.md` inside the VFS.
36. Configured host skill roots are mounted into the VFS at bootstrap time before discovery per S020; after mounting, the rest of the system resolves them exactly like project skills.
37. The registry stores the canonical VFS path to each discovered skill's `SKILL.md`.
38. Relative resources referenced by a skill are resolved relative to the directory containing that skill's `SKILL.md`.
39. Reading a supporting document from a skill directory requires an explicit `file_read` or `list_dir` call.
40. Executing a supporting script from a skill directory requires an explicit `shell_exec` or `js_exec` call.
41. Skills may reference resources outside their own directory only if the agent's normal path and execution capabilities allow it.

### Capability gating and active-skill behavior

42. Capability tokens are extended with skill patterns using the `skill:<name>` namespace and glob semantics. Config supports this through `[agent_types.<name>.capabilities] skill_patterns = [...]`. Empty `skill_patterns` retains the `CapabilityToken` default of allowing all skills; `agent_type.skills` remains the per-agent allow-list.
43. An agent's effective skill catalog is the intersection of:
   - the agent type's configured `skills` list;
   - the discovered skill registry;
   - the capability token's allowed `skill:<name>` patterns.
44. Capability checks happen at the call site: before returning a skill body, Simulacra verifies that the requested skill is allowed by the current capability token.
45. A skill never grants capabilities the agent does not already have. Skill prompts, `allowed_tools`, and supporting scripts are all constrained by the existing `CapabilityToken` and `ResourceBudget`.
46. `allowed_tools` only affects the interactive approval layer for the current turn. It does NOT alter `ToolRegistry`, does NOT bypass capabilities, and does NOT bypass budgets.
47. If multiple skills are loaded in one turn, their `allowed_tools` sets compose by union for that turn's approval logic.
48. Skill capabilities are attenuated like other capabilities: a child or narrowed token may expose a subset of the parent's skills but never a superset.

### Sub-agent skill inheritance

49. A child agent's available skills come from the child agent type's `skills` list, not from the parent's currently loaded skill bodies.
50. Child skill availability is still intersected with the parent's attenuated capability token and the child's own capability token.
51. A parent may delegate to a child that has no skills even if the parent currently has loaded skills in context.
52. Loaded skill bodies are not copied automatically into child conversations. If a child needs a skill, it must resolve that skill in its own context through its own effective catalog.

### Golden Rule and journaling

53. Skills remain prompt injections only. Any side effect suggested by a skill body must still execute through existing tools and `AgentCell`.
54. Model-triggered `Skill` calls are journaled and observed exactly like other tool calls under S005, S011, and S012.
55. User-triggered skill loads are recorded as host-side session events before the resulting turn is sent to the provider so the source of the injected prompt remains attributable.

## Assertions

### Discovery and config

- [x] Skills are discovered from project VFS `/skills/**/SKILL.md` paths and configured host skill mounts. **Implemented in `discover_and_filter_skills()` in `simulacra-tool/src/skills.rs` — recursively walks downward from `/skills` via VFS.**
- [x] Catalog-authored skills participate in the same VFS-first discovery path. **`SimulacraEngine` snapshots catalog skill documents into `/skills/<name>/SKILL.md` before calling `discover_and_filter_skills()`.**
- [x] `/skills` discovery remains rooted at `/skills` and does not walk upward or search sibling/ancestor `skills/` directories. **`discover_skill_paths()` starts at `/skills`; covered by `discovery_does_not_walk_up_or_search_for_other_skills_directories`.**
- [x] Nested grouped skill directories under `/skills` are supported. **Covered by `discovery_accepts_nested_skill_directories_under_skills_root`.**
- [x] The skill registry is keyed by frontmatter `name`, not directory name. **`discovered` HashMap is keyed by `meta.name` from frontmatter, not `dir_name`.**
- [x] Duplicate skill names across discovery roots fail startup instead of shadowing. **Returns `SkillError::DuplicateSkillName` when `discovered.contains_key(&meta.name)`.**
- [x] Invalid or missing `SKILL.md` frontmatter is skipped with a warning when unreferenced. **`tracing::warn!` emitted on parse failure; name added to `invalid_names` and skipped.**
- [x] An agent type that references an undiscoverable skill fails startup with an error naming the missing skill. **Returns `SkillError::UndiscoverableSkill { agent_type, skill }` when skill not in `discovered`.**
- [x] `agent_type.skills = ["..."]` restricts the skills exposed to that agent. **`discover_and_filter_skills` iterates only `agent_skills` list; `AgentTypeConfig.skills` field in `simulacra-config/src/lib.rs`.**

### `Skill` tool definition

- [x] Agents with at least one model-visible skill register exactly one built-in tool named `Skill`. **`SkillTool` struct implements `Tool` with `definition().name = "Skill"`. Doc comment: "registers exactly one built-in tool named Skill".**
- [x] Simulacra does not register separate tools for each skill. **Single `SkillTool` with a `catalog: Vec<SkillMeta>`, not per-skill tools.**
- [x] Agents with only user-invocable, model-disabled, or implicit-disabled skills do not expose an empty `Skill` tool definition to the model. **Assertion 14a in spec behavior; registration filters `!disable_model_invocation && allow_implicit_invocation`; if no model-visible skills remain, `Skill` tool is not registered.**
- [x] The `Skill` tool input schema requires `command` and rejects additional properties. **`definition()` returns schema with `"required": ["command"], "additionalProperties": false`.**
- [x] The `Skill` tool definition includes only skill `name + description`, not full `SKILL.md` bodies. **`build_catalog_description()` emits only `"- {name}: {description}"` entries.**
- [x] Skills with `disable_model_invocation: true` are excluded from the model-visible `Skill` catalog. **`build_catalog_description()` filters `.filter(|s| !s.disable_model_invocation && s.allow_implicit_invocation)`.**
- [x] Skills with `allow_implicit_invocation: false` are excluded from the model-visible `Skill` catalog. **`build_catalog_description()` filters `.filter(|s| !s.disable_model_invocation && s.allow_implicit_invocation)`.**
- [x] Skills with `user_invocable: false` may still remain model-invocable when `disable_model_invocation` is `false` and `allow_implicit_invocation` is `true`. **`user_invocable` field is not checked in `build_catalog_description`.**

### Skill file format

- [x] A valid skill directory requires `SKILL.md` with YAML frontmatter plus a markdown body. **`parse_skill_frontmatter()` returns errors for missing `---`, missing closing `---`, and empty body after frontmatter.**
- [x] The `name` field is the canonical identifier used by both `Skill(command=...)` and `/skill-name`. **`SkillMeta.name` used by `SkillTool::call()` for model lookup and by `dispatch_command()` for `/skill-name` resolution.**
- [x] The `description` field is exposed in the model-visible skill catalog. **`build_catalog_description()` formats `"- {name}: {description}"` for each model-visible skill.**
- [x] `disable_model_invocation: true` blocks model-triggered invocation. **`SkillTool::call()` returns error tool result when `skill.disable_model_invocation` is true.**
- [x] `allow_implicit_invocation: false` blocks model-triggered invocation without blocking `/skill-name`. **`SkillTool::call()` returns an error tool result for model calls when `allow_implicit_invocation` is false; interactive dispatch is still governed by `user_invocable`.**
- [x] `user_invocable: false` blocks `/skill-name` invocation. **`dispatch_command()` checks `s.user_invocable` in the `.find()` predicate; non-invocable skills fall through to unknown command.**
- [x] `allowed_tools` narrows interactive pre-approval only and does not widen capability policy. **`SkillMeta.allowed_tools` field exists; doc comments state "does NOT alter ToolRegistry, does NOT bypass capabilities, and does NOT bypass budgets".**

### Model-triggered invocation

- [x] `Skill(command="existing-skill")` returns the markdown body of `SKILL.md` with frontmatter removed. **`SkillTool::call()` reads via `cell.read_file`, calls `strip_yaml_frontmatter()`, returns body as `json!(body)`.**
- [x] `Skill` on an unknown skill returns an error tool result with a descriptive message. **Returns `json!({"is_error": true, "content": "unknown skill: ..."})` when not found in catalog.**
- [x] `Skill` on a capability-denied skill returns an error tool result that preserves the denial reason. **Calls `capability.check_skill()` and returns `json!({"is_error": true, "content": denied.reason})`.**
- [x] `Skill` on a model-disabled skill returns an error tool result even if the model guessed the name. **Checks `skill.disable_model_invocation` before capability check; returns error with "cannot be invoked by the model".**
- [x] Loading a skill does not automatically read sibling resource files. **`SkillTool::call()` reads only the `SKILL.md` file via `cell.read_file(&vfs_path)` — no sibling file access.**
- [x] Loading a skill does not automatically execute sibling scripts. **No script execution in `SkillTool::call()` — only reads and returns the skill body text.**
- [x] Multiple `Skill` calls in one turn load multiple skills independently. **Each `SkillTool::call()` resolves independently from the catalog; no shared state between invocations.**

### User-triggered interactive invocation

- [x] `/skill-name args` resolves a user-invocable skill and injects its body into the next turn context. **`dispatch_command()` in `session.rs` parses `/skill-name <args>`, finds skill in `skill_catalog`, pushes `skill.body` as a user message.**
- [x] The trailing `args` text after `/skill-name` is sent to the model as the user's instruction for that skill invocation. **`if let Some(user_args) = args { self.view.messages.push(user_message(user_args)); }`**
- [x] Built-in slash commands from S015 take precedence over skill names. **`dispatch_command()` matches built-in commands (`exit`, `clear`, `budget`, etc.) before the `_` arm where skill resolution occurs.**
- [x] Unknown or non-user-invocable skill names fall through to the existing S015 unknown-command behavior. **When `.find(|s| s.name == skill_name && s.user_invocable)` returns `None`, falls through to `"unknown command: /..."` message.**
- [x] Capability-denied skills are not invocable through `/skill-name`.
- [x] User-triggered skill loads are recorded before the provider turn executes. **`tracing::info!(simulacra.skill.source = "user", ...)` emitted in `dispatch_command()` before the skill body is injected.**

### Context budget

- [x] Skill metadata is capped to a configured percentage of the model context window when available, with an 8,000 character fallback budget. **`SkillTool::new()` uses `DEFAULT_SKILL_METADATA_BUDGET_CHARS`; `SkillTool::new_with_metadata_budget()` allows callers/tests to provide a budget.**
- [x] Only model-invocable skills count against the metadata budget. **`build_catalog_description()` filters `!s.disable_model_invocation && s.allow_implicit_invocation` before budget accounting.**
- [x] Metadata entries are considered in `agent_type.skills` order. **Catalog is built in `agent_type.skills` order by `discover_and_filter_skills`; `build_catalog_description` iterates in catalog order.**
- [x] Oversized descriptions are truncated before entire skills are omitted. **When an entry does not fit, `build_catalog_description()` first truncates the description to the remaining byte budget on a character boundary.**
- [x] Skills past the budget cutoff are omitted from the model-visible catalog instead of inflating the prompt. **When a truncated entry still cannot fit, the skill is skipped and `omitted` counter incremented.**
- [x] Omitted model-invocable skills cause the `Skill` tool description to indicate that the catalog is partial. **When `omitted > 0`, appends `"(catalog is partial — {omitted} additional skill(s) omitted due to metadata budget)"`.**
- [x] Omitted skills remain user-invocable when policy allows. **The catalog filtering and budget truncation only affect the tool definition text; the full `catalog` vec is still available for `/skill-name` resolution.**

### File resolution and resources

- [x] Project skills resolve from canonical VFS paths under `/skills`. **`discover_and_filter_skills()` walks `/skills/**/SKILL.md`; `SkillMeta.vfs_path` stores the canonical path.**
- [x] Configured host skill roots are mounted into the VFS before discovery. **S020 `process_host_mounts()` runs before discovery; configured mounts copy host skill roots into the VFS.**
- [x] The registry stores a canonical VFS path to each skill's `SKILL.md`. **`SkillMeta.vfs_path` field stores e.g. `"/skills/rust-dev/SKILL.md"`.**
- [x] Relative resource references resolve relative to the skill directory. **Skill body can reference sibling files by relative path; VFS path structure preserves directory hierarchy.**
- [x] Reading a supporting skill document still requires `file_read` or `list_dir`. **`SkillTool::call()` only reads `SKILL.md` — no automatic sibling file loading.**
- [x] Executing a supporting skill script still requires `shell_exec` or `js_exec`. **No script execution in `SkillTool` — doc comments explicitly state this requirement.**

### Capability gating and inheritance

- [x] The effective skill catalog is the intersection of discovered skills, `agent_type.skills`, and `skill:<name>` capability patterns. **`discover_and_filter_skills()` intersects: discovered map, `agent_skills` iteration, and `capability.check_skill()` gate.**
- [x] Configured `skill_patterns` map into `CapabilityToken` while empty still means allow all. **`CapabilitiesConfig.skill_patterns` is copied by `build_capability_token()`; `CapabilityToken::check_skill()` retains empty-as-allow-all semantics.**
- [x] Skill capability checks happen at the call site before a skill body is returned. **`SkillTool::call()` calls `capability.check_skill(&command)` before reading the file.**
- [x] A skill cannot grant access to a tool or path that the agent's capability token denies. **`allowed_tools` doc: "does NOT alter ToolRegistry, does NOT bypass capabilities, and does NOT bypass budgets".**
- [x] Multiple loaded skills union their `allowed_tools` for the current interactive turn only.
- [x] Child agents inherit only the skills listed on the child agent type, further attenuated by capability policy. **`discover_and_filter_skills()` takes the child's `agent_skills` list and child's `capability`; `is_subset_of()` enforces attenuation of `skill_patterns`.**
- [x] Loaded parent skill bodies are not copied automatically into child conversations. **Child `AgentLoop` is constructed independently in `AgentTaskFactory::create_task()` with its own context — no parent skill body injection.**

### Golden Rule and journaling

- [x] Skills remain prompt text only; all side effects still route through existing tools and `AgentCell`. **`SkillTool::call()` returns the skill body as a tool result string — no side effects. Doc: "prompt injection only — not a new execution surface".**
- [x] Model-triggered `Skill` calls are journaled like other tool calls. **`ToolRegistry::call()` wraps all tool invocations including `Skill` in a `tool_invoke` span with journal entries.**
- [x] User-triggered skill loads are recorded as host-side session events before provider execution. **`dispatch_command()` emits `tracing::info!(simulacra.skill.source = "user", ...)` before injecting the skill body into messages.**

## Observability (see S010 for conventions)

- [x] Each model-triggered `Skill` invocation produces a tool span with `gen_ai.tool.name = "Skill"`. **`ToolRegistry::call()` creates `info_span!("tool_invoke", gen_ai.tool.name = name)` which covers `Skill` calls.**
- [x] Skill invocation spans include `simulacra.skill.name` and `simulacra.skill.source` (`model` or `user`). **`SkillTool::call()` emits `tracing::info!(simulacra.skill.name, simulacra.skill.source = "model", ...)`. User path emits `simulacra.skill.source = "user"`.**
- [x] Skill resolution spans include the canonical VFS path of the loaded `SKILL.md`. **`tracing::info!(simulacra.vfs.path = %vfs_path, "skill loaded")` in `SkillTool::call()`.**
- [x] Skill capability denials emit a `WARN`-level event with the requested skill name and denial reason. **`tracing::warn!(skill_name, denial_reason, "skill capability denied")` in `discover_and_filter_skills()`.**
- [x] Bootstrap discovery emits an `INFO`-level event with discovered skill count and mounted skill-root count. **`tracing::info!(discovered_skill_count, mounted_skill_root_count, "skill discovery complete")` in `discover_and_filter_skills()`.**
- [x] User-triggered skill loads emit a tracing event linked to the interactive turn span before provider execution. **`tracing::info!(simulacra.skill.name, simulacra.skill.source = "user", linked = "interactive_turn", ...)` in `dispatch_command()`.**
