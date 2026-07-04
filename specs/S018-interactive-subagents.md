# S018 — Interactive Sub-Agent Spawning

**Status:** Active
**Crates involved:** `simulacra-cli`, `simulacra-runtime`, `simulacra-tool`, `simulacra-types`

## Dependencies

- **ARCHITECTURE.md** — Golden Rule, supervision model, capability attenuation, budget enforcement, OTel conventions
- **S005** — Journal (append-only, journal-before-return, replay/checkpoint rules)
- **S006** — Resource budgets (remaining budget is authoritative at the point of delegation)
- **S009** — Supervisor (actor loop, restart policy, capability attenuation, child budget accounting)
- **S012** — Built-in tools (tool registry, JSON Schema tool definitions, tool result flow)
- **S015** — Interactive mode (REPL loop, spinner/status line, one-turn-at-a-time integration with `AgentLoop`)

## Context

S015 defines interactive mode as a REPL built on `AgentLoop`. S009 defines a supervisor actor that can spawn, cancel, and restart child agents with attenuated capabilities and bounded budgets. This spec connects those two pieces for interactive sessions: the parent agent running in the REPL can delegate a sub-task to a child agent via a `spawn_agent` tool call, the interactive host supervises that child, and the parent can later join or cancel the live child by handle.

In interactive mode, `spawn_agent` is a non-blocking delegation primitive from the parent's point of view: the tool call returns after the child is accepted and started. Terminal child results are returned by `join_child_agent`; cancellation is requested with `cancel_child_agent`.

## Design

```text
User prompt
   |
   v
InteractiveSession (S015)
   |  owns parent AgentLoop messages
   |  starts one AgentSupervisor actor loop for the session
   |
   +--> parent AgentLoop::run_single_turn(...)
           |
           +--> ToolRegistry::call("spawn_agent", args, parent_capability)
                    |
                    +--> SpawnAgentTool
                           1. validate JSON args
                           2. derive child capability + budget
                           3. send SupervisorMessage::Spawn(Box<SpawnConfig>)
                           4. return accepted child handle
                    |
                    +--> AgentSupervisor::run_actor_loop(...)
                           |
                           +--> TaskFactory::create_task(config, cancellation)
                                   |
                                   +--> new child AgentLoop
                                         - fresh Provider instance
                                         - child ToolRegistry
                                         - child JournalStorage handle
                                         - child ResourceBudget
                           |
                           +--> child AgentLoop::run(task)
                           |
                           +--> terminal result cached for join_child_agent
                    |
                    +--> JSON child handle returned to parent AgentLoop
           |
           +--> ToolRegistry::call("join_child_agent", { child_id })
                    |
                    +--> SupervisorPayload::JoinChild(child_id)
                    +--> JSON terminal summary returned to parent AgentLoop
           |
           +--> parent continues with ToolResult message in conversation
```

## Behavior

### `spawn_agent` tool

1. Interactive sessions MUST register a tool named `spawn_agent` in the parent's `ToolRegistry` in addition to the tools defined by S012.
2. The tool definition presented to the LLM MUST use the following schema:

```json
{
  "type": "object",
  "properties": {
    "agent_type": {
      "type": "string",
      "description": "Configured agent type name from simulacra.toml to use for the child agent"
    },
    "system_prompt": {
      "type": "string",
      "description": "Inline system prompt for a generic child agent when agent_type is omitted"
    },
    "tier": {
      "type": "string",
      "description": "Optional configured tier for a generic child agent"
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
    "capabilities": {
      "type": "object",
      "description": "Optional attenuated capability override. If omitted, the child receives the configured capabilities for agent_type intersected with the parent's token.",
      "properties": {
        "network": {
          "type": "array",
          "items": { "type": "string" }
        },
        "mcp_tools": {
          "type": "array",
          "items": { "type": "string" }
        },
        "shell": { "type": "boolean" },
        "javascript": { "type": "boolean" },
        "python": { "type": "boolean" },
        "paths_write": {
          "type": "array",
          "items": { "type": "string" }
        },
        "paths_read": {
          "type": "array",
          "items": { "type": "string" }
        },
        "spawn_types": {
          "type": "array",
          "items": { "type": "string" }
        }
      },
      "additionalProperties": false
    }
  },
  "required": ["task", "budget"],
  "additionalProperties": false
}
```

3. `spawn_agent` MUST return a JSON object on accepted spawn with this shape:

```json
{
  "child_id": "agent-uuid",
  "agent_type": "researcher",
  "status": "running"
}
```

4. If validation or spawn acceptance fails before a child starts, `spawn_agent` MUST return an error tool result (`is_error: true` at the agent-loop boundary). If the failure is budget-related or capability-related, the error string MUST preserve the underlying reason.
5. `spawn_agent` is non-blocking from the parent model's perspective: the parent turn receives a live child handle immediately after the supervisor accepts the spawn. The parent receives exactly one handle result message for each `spawn_agent` tool call.
5a. `exit_reason` in the success result is one of: `"completed"`, `"budget_exhausted"`, `"max_turns"`. Only `"completed"` means the child finished normally. `"budget_exhausted"` and `"max_turns"` are partial-success results (child did work but hit a limit); these are NOT error results — the parent receives whatever the child produced. True failures (capability denied, spawn rejected, runtime error, cancellation) are error results with `is_error: true`.
5b. `spawn_agent` is auto-approved and MUST NOT require user confirmation. The parent agent's decision to delegate is sufficient — the child's capabilities are attenuated and budget-bounded by the host. The user sees the spawn in the transcript but is not prompted.
5c. The default restart strategy for interactive sub-agents is `LetCrash` — the child runs once, and if it fails the parent sees the error. No automatic restarts unless a future spec adds restart configuration to the tool schema.
5d. `join_child_agent` accepts `{ "child_id": "..." }`, waits for the child terminal result if needed, and returns the former `spawn_agent` terminal summary shape: `child_id`, `agent_type`, `exit_reason`, `message`, and `token_usage`.
5e. `cancel_child_agent` accepts `{ "child_id": "...", "reason": "optional" }`, signals that child only, and returns `{ "child_id": "...", "status": "cancel_requested" }`. Terminal cancellation is observed through `join_child_agent` and `ChildFinished`.

### Interactive host integration

6. `InteractiveSession` MUST create and own one `AgentSupervisor` for the lifetime of the interactive session. The supervisor actor loop MUST be started when the session is initialized, before the first parent turn is executed.
7. The interactive host MUST keep the supervisor actor loop alive across multiple parent turns in the same REPL session. It MUST NOT create a new supervisor per tool call.
8. The parent `spawn_agent` tool implementation MUST communicate with the supervisor through its actor message channel using `SupervisorMessage { agent_id: parent_agent_id, priority: MessagePriority::Command, payload: SupervisorPayload::Spawn(...) }` per S009.
9. `SpawnConfig` is extended for interactive sub-agent delegation. In addition to the S009 fields (`agent_id`, `parent_id`, `capability`, `budget`, `restart_strategy`), the spawned work item MUST carry:
   - `agent_type: String` — the configured child type name selected from `simulacra.toml`;
   - `task: String` — the delegated task text passed to `AgentLoop::run(task)`.
10. The supervisor actor loop remains a host-side concern. The LLM does not send supervisor messages directly and does not observe raw actor protocol messages.
11. Ctrl-C behavior from S015 extends to child execution: if the user interrupts while `join_child_agent` is waiting on a child, the interactive host MUST signal cancellation through the supervisor and the join result MUST be an error result with content indicating cancellation.

### Child result flow back to the parent

12. When the parent model emits `spawn_agent`, the parent `AgentLoop` journals the normal `ToolCall` entry first, as required by S012/S005.
13. The `spawn_agent` tool MUST return the accepted child handle without waiting for the child's terminal outcome.
14. The parent model sees the child outcome only through `join_child_agent`. The child's internal conversation history is NOT appended to the parent's message list.
15. The `join_child_agent` result content sent to the parent MUST contain only the child's terminal summary (`message`) and aggregate `token_usage`. Intermediate child tool results, internal reasoning, and child journal internals remain host-local unless the child includes them in its final assistant message.
16. If the child exits without a final assistant message, the joined `message` field MUST be an empty string rather than fabricating a summary.

### TaskFactory and child `AgentLoop` construction

17. Interactive mode MUST provide a `TaskFactory` implementation that constructs a fresh child `AgentLoop` for each spawn request.
18. `TaskFactory::create_task(config, cancellation)` MUST create a new `AgentLoop` with:
    - a fresh `AgentLoopConfig` using the child `agent_id`, model, max turns, and child capability token;
    - a fresh `Box<dyn Provider>` for the child agent type, not a reused in-flight provider instance from the parent;
    - a child `ToolRegistry` built from the standard built-ins plus `spawn_agent` only when the child type is allowed to spawn;
    - a context strategy instance for the child;
    - a journal storage handle scoped to the child `agent_id`;
    - the child `ResourceBudget` from the validated spawn request.
19. The provider for the child MUST be selected from the requested `agent_type` configuration in `simulacra.toml`. A child agent type may use a different model/provider configuration than the parent.
19a. The child's `system_prompt` is taken from the `agent_type` configuration in `simulacra.toml`. If the agent type has no `system_prompt`, the child uses the default system prompt (same as S013/S015).
20. `TaskFactory` MUST execute the child by calling `AgentLoop::run(task)` with the delegated `task` string from `SpawnConfig`. The child is a full agent loop, not a one-off provider call.
21. The child `AgentLoop` MUST be isolated from the parent's conversation state except for the delegated task text and inherited/attenuated execution context. Parent conversation messages are not implicitly copied into the child unless a future spec adds explicit context handoff.

### Capability attenuation

22. A child agent's effective capability token is:
    - the configured capability set for the requested `agent_type`,
    - intersected with the parent's capability token,
    - further intersected with the optional `capabilities` override when the tool call provides one.
23. The resulting child token MUST satisfy `CapabilityToken::is_subset_of(parent)` before the supervisor accepts the spawn, per S009 and ARCHITECTURE.md.
24. `agent_type` authorization MUST be enforced through configuration and capabilities together: the requested child type must be allowed by the parent's `can_spawn` configuration and must appear in the parent's effective `CapabilityToken.spawn_types`.
25. A child MUST NOT gain capabilities that the parent lacks, even if the child type's static config would otherwise allow them.
26. If attenuation fails, the child MUST NOT be started and the parent receives a capability-denied error tool result.

### Budget enforcement

27. The `budget` object in `spawn_agent` is a reservation request against the parent's remaining budget at the time of the tool call.
28. Before spawning, the supervisor MUST verify that the requested child budget does not exceed the parent's remaining `max_tokens`, `max_turns`, `max_cost`, or `max_sub_agents` headroom. Zero retains the meaning defined in S006: unlimited for that child dimension, but only if the parent dimension is also unlimited. **Implementation note:** the existing `spawn_agent()` in `AgentSupervisor` has a bug where `max_sub_agents == 0` and `max_tokens == 0` are treated as "already exhausted" instead of "unlimited". This MUST be fixed as part of S018: budget validation MUST skip the comparison when `parent.max_<resource> == 0`.
29. The supervisor MUST increment the parent's `used_sub_agents` count when the spawn is accepted.
30. When the child completes or fails after consuming resources, the child's actual `used_tokens`, `used_turns`, and `used_cost` MUST be rolled up into the parent's used budget so the parent's remaining budget reflects delegated work. **Implementation note:** the existing rollup reads from the stale `SpawnConfig.budget` clone, not the child's actual consumption. This MUST be fixed: rollup MUST use the child `AgentLoopOutput.token_usage` for token counts. `AgentLoopOutput` MUST be extended to carry `used_turns` and `used_cost` so full rollup is possible.
31. If the requested child budget cannot be reserved, the spawn MUST fail before the child task starts and the parent receives a budget-exhausted error tool result.
32. A child may itself spawn sub-agents only from its own remaining budget, not from the parent's original maximum budget.
33. `max_vfs_bytes` is not included in the `spawn_agent` tool schema. Child agents inherit `max_vfs_bytes: 0` (unlimited) by default. A future spec may add VFS budget controls.

### Journal

33. Journaling remains host-side and follows the Golden Rule: spawn and completion/failure journal entries are written before the corresponding result is returned to the parent tool call.
34. When a spawn is accepted, the host MUST append `JournalEntryKind::SubAgentSpawned { child_id, agent_type }` under the parent agent's journal before the child begins execution.
35. On successful child completion, the host MUST append `JournalEntryKind::SubAgentCompleted { child_id, success: true }` before `join_child_agent` returns its success JSON to the parent.
36. On child failure or cancellation, the host MUST append `JournalEntryKind::SubAgentCompleted { child_id, success: false }` before `join_child_agent` returns its error tool result to the parent.
37. The child agent maintains its own journal stream under `child_id` for its internal turns, tool calls, and results. Parent and child journals are linked by the parent's `SubAgentSpawned` / `SubAgentCompleted` entries and the shared `child_id`.
38. Replay of the parent journal MUST preserve the parent-visible tool result of `spawn_agent`; replay does not require re-running the child live if the corresponding parent `ToolResult` entry already exists.

### Configuration

39. `simulacra.toml` agent type definitions are extended with `can_spawn`, an allow-list of child agent type names:

```toml
[agent_types.default]
model = "claude-sonnet-4-6"
max_turns = 0
max_tokens = 0
can_spawn = ["researcher", "reviewer"]
```

40. `can_spawn` is session-start configuration owned by the host, not model-provided input. The model may request `agent_type`, but the host resolves authorization from config.
41. During config loading, `can_spawn` MUST populate the effective `CapabilityToken.spawn_types` for that agent type so spawn authorization remains enforceable at the call site.
42. If `can_spawn` is omitted, the default is an empty list: that agent type cannot spawn any children.
43. An agent type may only spawn types that are present in the loaded config. Unknown child types are rejected as invalid arguments before the supervisor starts work.

### Interactive UX

44. The interactive REPL MUST surface sub-agent activity as host output even though the parent model sees only a final tool result.
45. When `spawn_agent` begins, the existing spinner from S015 MUST continue, but its status text MUST identify sub-agent work (for example, `delegating to researcher...`).
46. Child-visible output rendered to the terminal MUST be prefixed with the child identity, for example `[agent:researcher/<child_id>]`, so the user can distinguish it from the parent assistant and from normal `[tool]` blocks.
47. The parent tool call block remains visible in the transcript as `[tool] spawn_agent: <arguments-json>` per S015. The final child result is rendered as the tool result that returns to the parent.
48. If multiple nested child agents are permitted in the future, each rendered line MUST retain the immediate child prefix; this spec does not require tree rendering beyond stable prefixes.
49. On child failure, the user MUST see an error line with the child prefix before control returns to the parent turn.
50. On child cancellation, the user MUST see a cancellation line with the child prefix and the parent turn resumes with an error tool result.

### Generic sub-agents (S023)

Generic sub-agent spawning is specified in S023. The `spawn_agent` tool accepts an optional `system_prompt` and `tier` in place of `agent_type`, allowing the parent to define sub-agent behavior at runtime.

## Assertions

### Tool definition and result shape

- [x] `spawn_agent` is registered in interactive sessions and appears in `/tools` output with the documented name and description. *(SpawnAgentTool registered in InteractiveSession; definition().name == "spawn_agent")*
- [x] The `spawn_agent` tool definition exposes `agent_type`, `task`, `budget`, and optional `capabilities` with a valid JSON Schema. *(SpawnAgentTool::definition() returns input_schema with all four properties and correct types)*
- [x] A successful `spawn_agent` call returns a tool result whose JSON content includes `child_id`, `agent_type`, and `status: "running"`.
- [x] `join_child_agent` returns a terminal result whose JSON content includes `child_id`, `agent_type`, `exit_reason`, `message`, and `token_usage`.
- [x] `cancel_child_agent` returns `status: "cancel_requested"` and causes the targeted child to observe cancellation.
- [x] Child runtime failures (supervisor errors, child panics) return `Err(ToolError::ExecutionFailed(...))` so the AgentLoop marks the result as `is_error: true`. Capability violations from `can_spawn` also return `Err(ToolError)`.

### Interactive host and actor integration

- [x] `InteractiveSession` starts one supervisor actor loop for the session and reuses it across multiple parent turns.
- [x] `spawn_agent` requests are sent to the supervisor as `SupervisorPayload::Spawn` command messages rather than bypassing the actor loop.
- [x] Ctrl-C while waiting on a child signals supervisor cancellation and returns an error tool result to the parent.

### Child execution and result flow

- [x] The parent receives exactly one handle tool result message per `spawn_agent` call.
- [x] Child internal messages are not appended to the parent's conversation history.
- [x] The child is executed through a full `AgentLoop::run(task)` invocation created by `TaskFactory`, not a raw provider call. *(AgentTaskFactory::create_task builds a child AgentLoop and calls child_loop.run(&task).await)*
- [x] Each child gets a fresh provider instance selected from the configured child agent type. *(AgentTaskFactory::create_task constructs a new Box<dyn Provider> from agent_type_config.model)*
- [x] `SpawnAgentTool` receives the parent's `AgentId` at construction and passes it as `SpawnConfig.parent_id`.

### Capability attenuation and config

- [x] The child effective capability token is the intersection of child type config, parent token, and optional tool override.
- [x] A parent cannot spawn a child type not listed in its `can_spawn` config. *(SpawnAgentTool::call checks can_spawn.contains(&agent_type) and returns ToolError if not; supervisor also checks parent_capability.spawn_types)*
- [x] `can_spawn` is reflected into the effective `CapabilityToken.spawn_types` used at spawn-time checks. *(build_capability_token sets spawn_types from agent_type.can_spawn in simulacra-config)*
- [x] A child spawn request that would widen capabilities beyond the parent is rejected before the child task starts. *(AgentSupervisor::spawn_agent calls config.capability.is_subset_of(&self.parent_capability) and returns CapabilityViolation on failure)*
- [x] The optional `capabilities` JSON argument from the `spawn_agent` tool call is parsed into a `CapabilityToken` and passed through `SpawnConfig.capability`.
- [x] Effective child capability = intersection of (child type config capability ∩ parent capability ∩ optional tool-call override).

### Budget enforcement

- [x] A child budget request exceeding the parent's remaining budget is rejected before child execution begins. *(AgentSupervisor::spawn_agent checks remaining token headroom when max_tokens > 0 and rejects if child max_tokens exceeds it)*
- [x] Accepting a child spawn increments the parent's `used_sub_agents`. *(spawn_agent does parent_budget.used_sub_agents += 1 after validation)*
- [x] Child token, turn, and cost usage are deducted from the parent's remaining budget when the child finishes. *(process_child_result rolls up used_tokens, used_turns, used_cost from AgentLoopOutput)*
- [x] A child may spawn descendants only from its own remaining budget.
- [x] Parent `max_sub_agents = 0` means unlimited sub-agents (not "already exhausted"). *(spawn_agent only checks used_sub_agents >= max_sub_agents when max_sub_agents > 0)*
- [x] Parent `max_tokens = 0` means unlimited tokens — child budget requests are not rejected. *(spawn_agent only checks token headroom when budget.max_tokens > 0)*
- [x] Budget rollup uses actual child consumption from `AgentLoopOutput`, not the stale `SpawnConfig` clone. *(process_child_result reads output.token_usage, output.used_turns, output.used_cost)*
- [x] `max_turns = 0` means unlimited turns for the parent (not already exhausted).
- [x] `max_cost = 0` means unlimited cost for the parent (not already exhausted).
- [x] Child `max_turns` exceeding parent remaining turns is rejected before child execution.
- [x] Child `max_cost` exceeding parent remaining cost is rejected before child execution.

### Approval, restart, and exit reason

- [x] `spawn_agent` is auto-approved (no user confirmation required) since child capabilities are attenuated and budget-bounded.
- [x] The default restart strategy for interactive sub-agents is `LetCrash`. *(SpawnAgentTool::call sets restart_strategy: RestartStrategy::LetCrash)*
- [x] `exit_reason: "budget_exhausted"` returns a success result (not error) with partial child output.
- [x] Child system_prompt comes from the agent_type config in simulacra.toml. *(AgentTaskFactory::create_task reads agent_type_config.system_prompt, falls back to DEFAULT_SYSTEM_PROMPT)*

### Journal

- [x] Parent journal records `SubAgentSpawned` before child execution begins.
- [x] Parent journal records `SubAgentCompleted { success: true }` before a successful `join_child_agent` result is returned.
- [x] Parent journal records `SubAgentCompleted { success: false }` before a failed or cancelled `join_child_agent` result is returned.
- [x] Child journal entries are written under the child `agent_id` and can be correlated to the parent by `child_id`.
- [x] Parent replay reuses the recorded `spawn_agent` tool result without requiring a live child run.
- [x] Supervisor actually appends `SubAgentSpawned` to the parent's journal stream before child execution begins (not just constructs the entry).
- [x] Supervisor actually appends `SubAgentCompleted` to the parent's journal stream after child completion/failure.

### Interactive UX

- [x] The REPL shows sub-agent work with a child-specific prefix distinct from parent assistant output and normal tool blocks.
- [x] The spinner/status text indicates delegation while a child is running.
- [x] Child failures and cancellations are shown to the user before the parent turn resumes.

## Observability (see S010 for conventions)

- [x] The parent `spawn_agent` tool invocation produces the normal tool span with `gen_ai.tool.name = "spawn_agent"` per S012.
- [x] Accepting a child spawn produces a `create_agent` span with `gen_ai.operation.name = "create_agent"` and `gen_ai.agent.name = <child_id>` per S009. *(AgentSupervisor::spawn_agent creates info_span!("create_agent") with both attributes)*
- [x] Running the child loop produces an `invoke_agent` span for the child agent per S009.
- [x] Sub-agent lifecycle spans include Simulacra-specific linkage attributes `simulacra.parent.agent_id` and `simulacra.child.agent_type`. *(create_agent span includes simulacra.parent.agent_id and simulacra.child.agent_type fields)*
- [x] Successful child completion is logged at `INFO` with child id, parent id, exit reason, and token totals. *(process_child_result emits tracing::info! with child_id, parent_id, exit_reason, token_total)*
- [x] Child failure is logged at `WARN` with child id, parent id, agent type, and failure reason. *(process_child_result emits tracing::warn! with child_id, parent_id, agent_type, failure_reason)*
- [x] Interactive sub-agent UX events (spawn started, child finished, child cancelled) are emitted as tracing events so terminal output can be correlated with lifecycle spans.
