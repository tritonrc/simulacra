# S060 — Task-Shaped Child Spawning

**Status:** Active — assertions proposed; implementation pending
**Crates involved:** `simulacra-types`, `simulacra-config`,
`simulacra-runtime`, `simulacra-tool`, `simulacra-cli`, `simulacra-server`

## Dependencies

- **ARCHITECTURE.md** — child composition, supervision, capability attenuation,
  journal-before-return, and the opaque ACP boundary
- **S004** — capability tokens and attenuation
- **S005** — journal envelope versioning and replay rejection
- **S006** — resource-budget limits and zero-means-unlimited semantics
- **S009** — supervisor ownership and lifecycle
- **S010** — observability conventions
- **S017** — skill discovery and progressive disclosure for native agents
- **S018** — supervised child handles and parent-facing lifecycle
- **S019** — child activity events and attribution
- **S023** — the current configured/generic spawn split
- **S026** — spawn governance-hook context and chaining
- **S034** — engine activity-event projection
- **S037** — memory capability remains host-supplied and attenuated
- **S054** — child status, wait, join, and list result shapes
- **S056** — native and ACP child runtime backends
- **S057** — child-local MCP activation and grants

The ownership and lifecycle clarifications below derive from
ARCHITECTURE.md's host-mediated capability checks and S009's supervisor
ownership model. Interactive tracking and cancellation retain S018/S054's
accepted-handle and terminal-result contracts. Recursive activity retains
S019's immediate-parent forwarding, and failure logging follows S010 without
adding a metric dimension.

## Why this spec exists

The current `spawn_agent` contract accidentally turns configured child profiles
into roles:

- `agent_type` is model-visible and may be emitted as a hard enum;
- the schema tells the model to prefer a configured type over caller shaping;
- `agent_type` and `system_prompt` are mutually exclusive;
- omitting `agent_type` always selects an in-process native child;
- only a configured profile whose backend is ACP can reach an injected ACP
  runtime.

The result is that a configured name controls placement, prompt, model, skills,
capabilities, and lifecycle policy as one indivisible identity. A caller can
shape an in-process child or obtain a workspace-backed child, but cannot do
both. Names such as `coder` and `research` then become de facto fixed roles even
though the child runtimes downstream are already role-free.

This spec replaces that two-mode model. The governing abstraction is:

```text
child = {
  placement,
  instructions,
  task,
  available skills,
  MCP capabilities,
  hooks,
  budget
}
```

Placement is not purpose. The host supplies an execution environment and an
attenuated capability envelope. The caller supplies the task and may shape how
the child approaches it. The child chooses its workflow from the skills and
tools available in that environment.

## Terminology

### Placement

A **placement** is an opaque configured child-runtime profile. It is stored in
`SimulacraConfig.child_placements` as a `ChildPlacementConfig`, then selects:

- `backend = "native"`: an in-process Simulacra `AgentLoop`; or
- `backend = "acp"`: the opaque injected `AcpChildRuntime` boundary.

An embedding may interpret an ACP placement as a workspace pod, but Simulacra
does not inspect or promise that execution location. Placement resolution
combines a configured runtime profile with host-composed MCP and governance-hook
dependencies. Its exact configuration shape is:

```rust
#[serde(deny_unknown_fields)]
struct ChildPlacementConfig {
    backend: AgentBackend,                 // default: Native
    model: Option<String>,
    acp_profile: Option<String>,
    skills: Vec<String>,                   // default: []
    capabilities: Option<CapabilitiesConfig>,
    max_turns: Option<u32>,
    max_tokens: Option<u64>,
    max_cost: Option<Decimal>,
    max_sub_agents: Option<u32>,
    allowed_child_placements: Vec<String>, // default: []
}
```

The fields mean:

- `backend`: `native` (the default) or `acp`;
- `model`: required and non-blank for `native`, absent or blank for `acp`;
- `acp_profile`: required and non-blank for `acp`, absent for `native`;
- `skills`: the ordered native skill allow-list from S017;
- `capabilities`: the configured capability envelope, including MCP and skill
  patterns;
- `max_turns`, `max_tokens`, `max_cost`, and `max_sub_agents`: optional
  placement maxima; a TOML `max_cost` is a decimal string;
- `allowed_child_placements`: descendant-spawn authorization.

The config structs deny unknown fields. Unknown `backend` values fail with an
error that names the rejected value and the allowed values `native` and `acp`.
Omitted `backend` resolves to `native`. A native placement requires non-blank
`model` and rejects a non-blank `acp_profile`; an ACP placement requires
non-blank `acp_profile` and rejects a non-blank `model`.
Global MCP server definitions and the injected hook pipeline remain
host-composed dependencies; their effective grants are constrained by the
placement capability envelope. These are capabilities and constraints, not a
role definition.

`ChildPlacementConfig` deliberately has no system prompt or instructions field.
Placement configuration cannot author workflow. Root-agent configuration under
`[agent_types.*]` remains a separate concept and is never consulted to resolve a
child placement.

As a clean-break authorization rename, root `AgentTypeConfig` replaces
`can_spawn` with `allowed_child_placements`; it maps to the root capability
token's `spawn_placements`. No root prompt, model, skill, or other agent-type
behavior moves into child placement configuration.

Root agents remain native. `backend` and `acp_profile` are removed from
`AgentTypeConfig` and rejected as unknown `[agent_types.<name>]` fields; those
names exist only on `ChildPlacementConfig`. S060 does not add an ACP root-agent
runtime.

Placement keys SHOULD describe execution or capability profiles, such as
`in_process` and `workspace`. They SHOULD NOT describe jobs such as `coder`,
`researcher`, `reviewer`, or `validator`.

### Instructions and task

`instructions` is caller-supplied shaping text: how the child should approach
this delegated task. `task` is the concrete work to perform. Instructions may
name relevant available skills, evidence standards, boundaries, or reporting
requirements. Instructions do not grant a skill, tool, filesystem, network, or
spawn capability.

For a native child, effective instructions occupy the child system-prompt
position. For an ACP child, Simulacra passes instructions and task as distinct
fields to the injected runtime. The embedding owns their protocol-specific
delivery while preserving their distinction and order. Simulacra does not
pretend that ACP exposes a native system-message channel.

### Available skills

Skill availability remains a host-supplied capability:

- native placements retain S017 discovery, allow-list, and capability
  intersection semantics;
- ACP skill delivery remains opaque to Simulacra;
- an embedding that delivers every discovered skill to every ACP child may
  continue doing so unchanged.

This version does **not** add a `skills` allow-list to `spawn_agent`. Per-spawn
skill filtering would let the harness dictate workflow again and would break
embeddings whose role-free bootstrap supplies the whole catalog. A caller that
wants the child to use a particular skill names it in `instructions`; the child
still decides whether and how to load it.

## Model-facing contract

The `spawn_agent` schema is a flat object:

```json
{
  "type": "object",
  "properties": {
    "placement": {
      "type": "string",
      "description": "Where I should run this child and which host-supplied capability envelope it receives. This selects placement, not a role. Available placements: <sorted JSON-quoted keys>."
    },
    "instructions": {
      "type": "string",
      "description": "How I should shape this child for the delegated task, including any relevant available skills and evidence requirements. This does not grant capabilities."
    },
    "task": {
      "type": "string",
      "description": "The concrete, bounded work I should hand to the child."
    },
    "budget": {
      "type": "object",
      "description": "The maximum resources I should reserve for this child; each nonzero value must fit within my remaining budget and the placement limits, while zero requests unlimited capacity under the rules below.",
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
      "description": "Capabilities I should remove from this child's placement envelope; these values can only attenuate access.",
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
}
```

`<sorted JSON-quoted keys>` is generated from `allowed_placements`, sorted by
Unicode scalar value and joined by `, `. It is descriptive discovery, not an
enum or a recommendation. If the list is empty, the final sentence is exactly
`No child placements are available in this session.` instead. The tool may be
omitted from a registry whose effective list is empty; if it is registered,
every call still fails authorization.

The empty-list sentence replaces only the `Available placements: ...` sentence;
the first two placement-description sentences remain unchanged.

The contract changes are:

1. `placement` replaces model-visible `agent_type`.
2. `instructions` replaces model-visible `system_prompt`.
3. `placement` and `instructions` are independent and may be supplied together.
4. The schema does not emit a placement enum. Authorization remains a runtime
   check against the host-provided allow-list and capability token. This avoids
   teaching an accidental role vocabulary as a closed ontology.
5. The top-level schema remains an ordinary object. No top-level `oneOf`,
   `anyOf`, or `allOf` is permitted.
6. Generated descriptions do not recommend one placement over caller shaping.
7. Unknown fields are rejected inside `budget` and `capabilities`, as well as
   at the top level.

`skill_patterns` and `memory` are deliberately absent from the model-facing
capability override. They remain placement/parent-supplied capabilities and are
attenuated by their existing S004/S017/S037 rules; a spawn call cannot select or
widen them.

The default tool description is exactly this string (the displayed line is the
whole value):

```text
I can start a supervised child for one concrete, bounded, independent task. Choose where I run it with placement and shape how it works with instructions; placement supplies an environment and capabilities, not a role. I return a live handle, not the child's final answer.
```

Host-provided `SpawnAgentGuidance` may still replace the tool-level description
and acknowledgement note verbatim. It does not replace any property description.
Simulacra's generated property descriptions remain the exact strings shown
above, with only the documented placement-list substitution.

An accepted spawn acknowledgement contains exactly `child_id`, `placement`, and
`status: "running"`. If and only if host guidance supplies a result note, the
acknowledgement additionally contains that verbatim `note` field.

## Validation and resolution

Validation stays in ordinary runtime code rather than schema combinators.

1. `placement`, `task`, and `budget` are required for the new model-facing
   contract. Empty or unknown placement keys fail before a child handle is
   accepted.
2. `instructions` is optional. Validation first checks the raw UTF-8 byte
   length, then treats a value whose Unicode `trim()` result is empty as absent.
   A non-blank value is preserved byte-for-byte, including leading and trailing
   whitespace, and shapes this child independently of its placement profile.
3. When caller instructions are absent:
   - a native placement uses the native default prompt;
   - an ACP placement passes no shaping instructions and remains task-only.
4. Caller instructions are bounded at 65,536 UTF-8 bytes. An oversized value is
   rejected before spawn with the actual and maximum byte counts. This replaces
   S023's 8,192-byte bound so a caller-supplied shaping prompt has parity with
   the embedding's existing bounded agent material.
5. `tier` is absent from the model-facing schema and rejected at runtime. Model
   and ACP profile selection belong to the placement configuration.
6. The selected placement is checked against both the tool's host allow-list
   and the caller's effective spawn capability before budget reservation,
   journaling, acknowledgement, or child construction.
7. Unknown placement configuration is rejected synchronously. It must not
   produce a running acknowledgement followed by an immediate asynchronous
   child failure.
8. Invalid-argument errors name the rejected field. Unknown-placement errors
   name the requested key and the sorted available keys, when any exist.
   Unknown-backend config errors name the rejected value and both allowed
   values. Size errors report actual and maximum UTF-8 byte counts. These are
   the required criteria for an actionable error in this spec.
9. A missing, empty, or Unicode-whitespace-only `task` is rejected. Every other
   task value is preserved byte-for-byte.
10. Budget value `0` retains S006's meaning of unlimited. A zero request is
    accepted only when the parent's corresponding maximum is also unlimited and
    the placement maximum is absent or zero. Otherwise it fails before hooks or
    reservation, naming the field and the finite parent or placement limit.
11. A nonzero budget request must not exceed the parent's remaining value or a
    finite placement maximum. It is rejected rather than clamped, before hooks
    or reservation, with the field, requested value, and limiting value. An
    absent or zero placement maximum is unlimited; placement maxima never raise
    or silently lower a caller request.
12. The public supervisor is explicitly bound to one root agent id by the host
    before its actor loop starts or any direct spawn or child-control operation
    is exposed. An unbound supervisor rejects every spawn and parent-facing
    child-control operation before factory validation/preparation/construction,
    hooks, budget reservation or charging, journal or activity emission, task
    launch, or mutation of child/result/input/cancellation/reservation maps.
    Rejection does not bind the supplied caller: there is no first-caller grant.
    Accepted child metadata remains the only authority by which a bound root's
    descendants authenticate as immediate callers. Production CLI and server
    construction bind the configured root id before starting the actor.

## Runtime construction

The factory dispatch decision is placement-first:

```text
placement -> configured profile -> backend
                               +-> native AgentLoop
                               +-> injected AcpChildRuntime
```

Caller instructions never select the backend. Backend never selects the
child's purpose.

### Native placement

- Resolve model, capability envelope, available skills, MCP grants, and budget
  defaults from the placement profile. Use the host-composed hook pipeline.
- Preserve S018's `LetCrash` spawn behavior; restart policy is not a placement
  field or model input in S060.
- Intersect placement capabilities with parent capabilities and any caller
  attenuation exactly as today.
- Use caller instructions when provided; otherwise use the default native
  prompt. Do not read the placement profile's configured prompt for a
  new-contract child.
- Keep descendant spawning controlled by the effective placement capability,
  not by inferred purpose.

### ACP placement

- Preserve every S056 boundary invariant: no native VFS, `AgentCell`, provider,
  tool registry, or sandbox construction.
- `AcpChildRequest` is serializable and has this complete field set:

  ```rust
  #[derive(Debug, Clone, Serialize, Deserialize)]
  struct AcpChildRequest {
      child_id: AgentId,
      parent_id: AgentId,
      placement: String,
      acp_profile: String,
      instructions: Option<String>,
      task: String,
      budget: ResourceBudget,
      capability: CapabilityToken,
  }
  ```

  It has no skill selector, MCP manifest, hook payload, workspace, VFS,
  provider, or sandbox field.
- `AcpChildRuntime::start_child` continues to receive `CancellationToken`,
  `ActivitySink`, and the live `AgentInputQueue` as method parameters. They are
  not duplicated inside `AcpChildRequest`.
- Do not inspect the embedding's workspace, skill catalog, MCP manifest, hooks,
  or system-prompt mechanism.
- With no caller instructions, preserve task-only request behavior.

## Authorization vocabulary

Child-spawn authorization uses placement vocabulary end to end:

- `SpawnAgentTool.allowed_placements` is the sorted, deduplicated intersection
  of configured placement keys and the parent's effective
  `spawn_placements`; it describes and validates the tool surface;
- `CapabilityToken.spawn_placements` is the caller's effective authorization;
- a placement may declare `allowed_child_placements` for descendants.

The child-spawn path does not read `can_spawn` or `spawn_types`. Root-agent
configuration may keep unrelated agent-type policy, but child placement
authorization has no role/type alias.

Authorization continues to be redundant at the call site and supervisor
boundary. The rename must not turn an empty list into an allow-all bypass or
permit an unconfigured/generic path to evade placement authorization. An empty
placement allow-list means the caller cannot spawn; it never means allow all.

## Clean break

S060 provides no compatibility aliases or fallback mode:

- `agent_type`, `system_prompt`, and `tier` are rejected tool arguments;
- every spawn names a configured `placement`;
- there is no unconfigured/generic native spawn path;
- child configuration is read only from `child_placements`, never
  `agent_types`;
- acknowledgements, child-control results, journal entries, activity events,
  errors, hooks, and telemetry use `placement`, never `agent_type`;
- `JOURNAL_SCHEMA_VERSION` increments from 2 to 3 and every `JournalEntry`
  retains its per-entry `schema_version`. Every append/read/query/fork/replay
  path requires strict equality with 3. On read, it inspects the per-entry
  version before deserializing that entry's `JournalEntryKind`; any other value
  returns `SchemaVersionMismatch { expected: 3, got }` and the entry is not
  decoded, counted, copied, or replayed. Rejection logs expected/got at `ERROR`
  and tells the operator to start a new session. A v3 payload with the old
  `SubAgentSpawned` fields is malformed rather than inferred or adapted.

Child ids have the exact form `child-` followed by 32 lowercase hexadecimal
digits. They are generated by the host, are pairwise unique within the process,
and are never derived from placement, task, or instructions. The caller cannot
supply an id. Supervisor ownership continues to prevent one parent from
controlling another parent's child.

The model-facing schema contains no `child_id` input, and an unknown top-level
`child_id` is rejected like every other unknown argument. `SpawnConfig.agent_id`
is an internal host boundary only. Both the direct and actor spawn paths reject
an internal id that was already accepted by that supervisor, including a
running, terminal, or explicitly closed child id. Duplicate rejection is
synchronous and occurs before factory validation/preparation/construction,
hooks, budget reservation or charging, journal or activity emission, task
launch, or mutation of any supervisor map. Every pre-existing child handle,
task, cancellation/input handle, budget account/reservation, cached result, and
delivery state remains byte-for-byte and behaviorally unchanged by the rejected
attempt.

## Journaling, hooks, activity, and telemetry

- `SubAgentSpawned` has exactly `child_id`, `placement`, `backend`, `task`, and
  `instructions: Option<String>`. It records the effective values after hook
  attenuation and before child execution.
- The spawn before-hook context is exactly `{ placement, backend, instructions,
  task, budget, capabilities }`; absent instructions serialize as `null`.
  `budget` is the caller request after parent/placement-limit validation.
  `capabilities` is never `null`: it is the full effective placement ∩ parent ∩
  optional caller attenuation calculated before the hook chain.
  Before-hooks run after ordinary argument/config validation but before budget
  reservation, journaling, acknowledgement, or child construction. `Deny` and
  `Kill` therefore leave no accepted child or `SubAgentSpawned` entry.
- A before-hook modified context may only attenuate `budget` and `capabilities`.
  The runtime rejects changes to `placement`, `backend`, `instructions`, or
  `task`, rejects unknown fields, and rechecks budget and capability attenuation
  after the whole before-chain. Capability modifications must be a subset of the
  pre-chain effective value; adding a caller-omitted capability field is not a
  grant. This lets governance constrain execution without authoring the child's
  workflow.
- Spawn-hook `HookDenial` and `HookKill` entries are appended to the parent's
  journal stream. A before-hook `Deny` returns a tool error without terminating
  the parent. A before-hook `Kill` propagates to the spawning parent agent loop,
  which terminates with `exit_reason: "PolicyKill"` per S026.
- Once a spawn before- or after-hook produces `Kill`, that policy decision is
  authoritative. Appending its `HookKill` audit entry is mandatory, but an
  append failure does not change, downgrade, or erase the resulting
  `PolicyKill`; in particular, it must not become `Continue`, `Deny`, or an
  ordinary tool/journal error. The append failure is reported to the host with
  an `ERROR` log and an error recorded on the active hook/spawn span, including
  the hook, phase, parent id, and append error. A before-hook kill still leaves
  zero accepted-spawn effects. An after-hook kill still preserves the child's
  original typed settlement and does not rewrite its status or exit reason.
  This composes narrowly with ARCHITECTURE.md's Journal Before Return invariant:
  the failed `HookKill` append cannot authorize or undo policy, while the
  separate `SubAgentSpawned` and `SubAgentCompleted` entries remain mandatory
  before their corresponding accepted handle or terminal child result can
  become parent-visible. Failure to append either lifecycle entry therefore
  still prevents return of the corresponding unjournaled lifecycle result.
- The spawn after-hook context is exactly `{ child_id, placement, backend,
  result, tokens_used }`. `result` is the terminal status string `completed`,
  `failed`, or `cancelled`; `tokens_used` is an unsigned integer. After-hooks run
  after the child runtime returns and before the supervisor caches its typed
  terminal result or appends `SubAgentCompleted`. Their normal reverse-order
  chaining remains intact, but a modified JSON context does not rewrite that
  typed terminal result. An after-hook `Kill` signals the spawning parent to
  terminate with `PolicyKill`; the supervisor still caches and journals the
  child's original terminal result.
- Child settlement uses this exhaustive `ExitReason` mapping. This is the S018
  child-result mapping; S034's `ExitReason` to `TaskState` table continues to
  govern root server tasks and does not redefine child settlement:

  | Child `ExitReason` | Child status / after-hook `result` | `SubAgentCompleted.success` | Terminal |
  |---|---|---:|---:|
  | `Complete` | `completed` | `true` | yes |
  | `MaxTurns` | `completed` | `true` | yes |
  | `BudgetExhausted` | `completed` | `true` | yes |
  | `Error(_)` | `failed` | `false` | yes |
  | `GuardrailTripped(_)` | `failed` | `false` | yes |
  | `PolicyKill { .. }` | `failed` | `false` | yes |
  | `Cancelled` | `cancelled` | `false` | yes |
  | `AwaitingApproval` | nonterminal (`running`, `ready: false`) | no entry | no |

  `AwaitingApproval` is the nonterminal compatibility state defined by S034:
  it must not run spawn after-hooks, emit `ChildFinished`, cache/deliver a
  terminal result, or append `SubAgentCompleted`. The same accepted child
  remains owned and resumable through its approval channel. Under S054's
  unchanged child-status wire contract, status/list expose it as `running` with
  `ready: false`, a bounded wait may time out with the ordinary running result,
  and join remains pending until that same child resumes and reaches one of the
  terminal rows above (or is cancelled). Backends must not report
  `AwaitingApproval` as a final child-runtime result.
- Runtime `ActivityEvent` child variants use the field name `placement`.
  Engine top-level child lifecycle projections also use `placement`; recursive
  flattened child attribution uses `child_placement`. No child lifecycle shape
  contains `agent_type` or `child_agent_type`. The existing `child_task`
  projection is retained unchanged. Recursive activity preserves one wrapper
  per supervision hop: each child's events are wrapped by that child's
  immediate parent before forwarding, so a grandchild event reaches the root as
  root-child `ChildActivity` containing grandchild `ChildActivity` containing
  the original event. No hop is flattened, skipped, or rewritten as though the
  root directly owned every descendant.
- The `create_agent` span records bounded
  `simulacra.child.placement` and `simulacra.child.backend` attributes.
- Raw instructions, task text, skill names, or caller-supplied focus labels are
  never metric labels.
- Logs record instruction length, not instruction contents. The journal remains
  the audit surface for full shaping text.
- A child-execution failure log is `WARN` and may identify the opaque
  `child_id`, `parent_id`, and placement, but its only error-descriptive field is
  `error_category`. That field is exactly one of the bounded values `provider`,
  `acp_runtime`, or `runtime`: a native provider error maps to `provider`, an
  error returned by the opaque ACP child-runtime port maps to `acp_runtime`, and
  every other child-execution error maps to `runtime`. The log never includes a
  raw provider, ACP, or runtime error string, task or instruction text, or skill
  name. `error_category` is a log field only; S060 does not add it or any other
  failure value as a metric label.

## Interactive accepted-child tracking

The interactive host tracks every accepted, still-live opaque child id, not a
single most-recent child slot. `ChildSpawned` inserts that exact accepted id;
`ChildFinished` removes only the exact matching id and leaves every concurrent
child tracked. Ordering used for display is deterministic insertion order, but
ordering never grants ownership or selects a cancellation target.

S018's Ctrl-C-during-join behavior uses an explicit selected child: while a
single-child `join_child_agent` is in flight, the selected id is the exact
`child_id` from that validated call. With no such explicit single-child
operation, no child is implicitly selected from activity order; in particular,
the host does not cancel the latest spawned or lexically first child. To cancel
the selected child, the interactive host sends signal-priority
`SupervisorPayload::CancelChild(selected_child_id, result_tx)` and awaits the
supervisor acknowledgement. It renders cancellation-requested output only
after `result_tx` returns `Ok(())`. An error acknowledgement is surfaced as an
error and must not render or return a claim that cancellation was requested;
terminal `cancelled` output still waits for the child's actual terminal result
and `ChildFinished` as required by S018.

## Roster guidance for embeddings

Simulacra does not own DevForge's `<conversation-state>` renderer, but its new
lifecycle contract makes a role-free projection possible. Embeddings should
render either:

```text
- <child_id> — <status> for <elapsed>
```

or, when location matters:

```text
- <child_id> [workspace|in_process] — <status> for <elapsed>
```

They should not render placement profile keys as a child's identity. If
concurrent children need human/model-readable differentiation, an embedding may
persist a separate bounded, escaped `focus` label. It must not inject the raw
task or instructions into every turn, and it must not use a free-form focus
value as a telemetry dimension.

## Superseded behavior

S060 supersedes these earlier clauses:

- **S004:** only the `CapabilityToken.spawn_types` member becomes
  `spawn_placements`, meaning configured child placements the holder may spawn.
  Empty remains deny-all. All proxy enforcement, subset, denial, journal, and
  OTel requirements remain.
- **S005:** S060's version-3 clean break supersedes the general requirement to
  read older journal versions. Version 2 and earlier are rejected before entry
  decoding; they are not migrated or partially replayed. All append-only,
  ordering, error-reporting, and observability requirements remain.
- **S017:** root agents continue using `[agent_types.<name>].skills`. Native
  S060 children instead use `[child_placements.<name>].skills`, and their
  effective catalog is placement skills ∩ discovered skills ∩ effective
  `skill_patterns`, ordered by the placement list. Loaded parent skill bodies
  are not copied. All discovery, progressive-disclosure, invocation, metadata
  budget, and call-site capability rules remain. ACP skill delivery is opaque.
- **S018:** replace the complete old `spawn_agent` argument schema,
  acknowledgement `agent_type`, terminal-result `agent_type`, `SpawnConfig`
  type/prompt/tier fields, configured-type provider/prompt resolution,
  `can_spawn`/`spawn_types` authorization, `SubAgentSpawned` shape, child
  type-based rendering, child type observability, and raw failure-reason log
  content with S060's placement and bounded failure-log contracts. Tool
  registration, live handles, fresh child construction, isolation, supervision,
  budgets, cancellation, steering, completion, journal-before-execution, and
  parent/child journal linkage remain.
- **S019:** the three child variants are `ChildSpawned { child_id, placement,
  task }`, `ChildActivity { child_id, placement, event }`, and `ChildFinished {
  child_id, placement, exit_reason, duration_ms, tool_uses, token_count }`.
  Renderers identify blocks by opaque child id or the generic word `Child`, not
  by placement. Recursive forwarding, ordering, statistics, serialization, and
  correlation remain.
- **S023:** retire the configured/generic spawn split, its argument schema and
  XOR validation, inline tier selection, 8 KiB prompt limit, generic native
  leaf construction, and configured/generic telemetry. The `[tiers]` parser and
  any non-child consumers remain valid; S060 child spawning never consults it.
  Parent budget consumption/rollup, actor flow, caller capability attenuation,
  activity emission, trace nesting, and S006's zero-means-unlimited runtime
  meaning apply to all placement-backed children, subject to S060's explicit
  parent/placement validation for zero spawn requests.
- **S026:** replace only the spawn before/after context examples and the effect
  of spawn-context modification with S060's exact contexts and validation
  rules. Hook ordering, chaining between hooks, first-deny-wins, timeouts,
  denial/kill journaling, and OTel behavior remain.
- **S034:** runtime and top-level child lifecycle projections use `placement`;
  recursively flattened attribution uses `child_placement`. Root
  `TenantConfig.agent_type`, root-agent resolution, and root task
  observability remain unchanged.
- **S054:** acknowledgements, stored child metadata, status/wait/join/list
  results, and host inspections replace `agent_type` with `placement`. Running
  wait results also include placement. Cached-result and `result_delivered`
  semantics, deterministic ordering, wait-any, close, and all other lifecycle
  fields remain. Registry exposure derives from effective
  `spawn_placements`, not configured-child versus generic-leaf categories.
- **S056:** child backend configuration moves from agent types to placements;
  parent-facing tool schemas/results follow S060; spawn hooks follow S060; and
  the ACP port uses S060's complete request. ACP opacity, supervisor ownership,
  cancellation, live steer delivery, result/usage handling, VFS independence,
  error classification, and transport/artifact non-goals remain.

All lifecycle, supervision, cancellation, steering, budget, attenuation,
journaling-before-return, and opaque ACP execution invariants remain in force.

## Non-goals

- Selecting, filtering, or withholding ACP manifest skills per child.
- Parsing skill names out of instructions.
- Allowing instructions to widen capabilities.
- Standardizing workspace pods as a Simulacra concept.
- Defining how an ACP implementation maps instructions into a particular ACP
  provider's protocol or message roles.
- Selecting MCP catalogs or hooks from model input.
- Rewriting embedding-owned skills, prompts, roster storage, or UI in this
  Simulacra task.

## Assertions

### Configuration and vocabulary

- [ ] `SimulacraConfig` parses configured child placements from
  `[child_placements.<name>]` into the exact `ChildPlacementConfig` field set in
  this spec, and unknown fields are rejected.
- [ ] Omitted backend resolves to native; `backend = "remote"` fails naming its
  config path, offending value, and allowed values `native`/`acp`.
- [ ] Native without a non-blank model, native with a non-blank ACP profile, ACP
  without a non-blank ACP profile, and ACP with a non-blank model each fail
  naming the placement path, offending field, and backend-specific requirement.
- [ ] A child placement containing `system_prompt` or `instructions` fails and
  names `child_placements.<name>.<field>` as unknown.
- [ ] Root `allowed_child_placements` and placement
  `allowed_child_placements` populate `CapabilityToken.spawn_placements`, while
  `can_spawn` and `spawn_types` are rejected rather than aliased.
- [ ] Root `[agent_types.<name>]` rejects `backend` and `acp_profile`, while the
  same fields validate under `[child_placements.<name>]` by the placement rules.
- [ ] A placement key that also exists under `[agent_types.*]` resolves only
  from `[child_placements.*]`; deleting the placement entry makes spawn fail
  even when the agent-type entry remains.

### Tool definition and validation

- [ ] The emitted `spawn_agent` schema is a flat object requiring `placement`,
  `task`, and `budget`, with optional `instructions` and `capabilities`, and its
  complete nested budget/capability schemas equal this spec.
- [ ] The emitted schema contains no `agent_type`, `system_prompt`, `tier`,
  `child_id`, `skills`, `skill_patterns`, `memory`, or placement enum and no
  top-level `oneOf`, `anyOf`, or `allOf`; a model call containing `child_id` is
  rejected as an unknown field.
- [ ] With allowed placements `workspace`, `in_process`, and duplicate
  `workspace`, the emitted placement description contains the sorted list
  `"in_process", "workspace"`, contains no enum, and otherwise equals the
  normative description; an empty list emits the normative no-placements
  sentence and authorizes nothing.
- [ ] The default tool description and the `placement`, `instructions`, `task`,
  `budget`, and `capabilities` descriptions equal the normative strings in this
  spec; host guidance changes only the tool-level description.
- [ ] Runtime calls containing `agent_type`, `system_prompt`, `tier`, or unknown
  top-level fields fail before spawn and name the rejected field.
- [ ] Unknown nested budget/capability fields and
  `capabilities.spawn_types` fail before spawn and name the rejected field.
- [ ] Empty, missing, unknown, or unauthorized placement fails before budget
  reservation, journaling, acknowledgement, hooks, or child construction; both
  the tool and supervisor independently deny an unauthorized placement.
- [ ] Missing, `""`, `" "`, and `"\n\t"` instructions all become `None`; a
  non-blank value preserves surrounding whitespace byte-for-byte through native
  construction, ACP delivery, hooks, and journaling.
- [ ] A 65,536-byte non-blank value is accepted; 65,537 non-blank bytes and
  65,537 whitespace bytes are rejected before spawn with actual and maximum
  byte counts.
- [ ] Missing, empty, and whitespace-only task values fail before hooks or
  reservation; a non-blank task preserves surrounding whitespace byte-for-byte.
- [ ] For each budget dimension, a zero request succeeds only when both parent
  and placement are unlimited; otherwise it fails with the field and finite
  limit. A nonzero request above parent remaining or a finite placement maximum
  is rejected rather than clamped, while a request at each boundary is accepted.

### Runtime placement and shaping

- [ ] A native placement with instructions constructs an in-process child using
  those instructions as its system prompt and the delegated task unchanged.
- [ ] A native placement without instructions uses the native default prompt.
- [ ] An ACP placement with instructions calls the injected `AcpChildRuntime`
  with distinct, byte-identical `instructions` and `task` fields.
- [ ] An ACP placement without instructions passes `instructions: None` and the
  delegated task unchanged.
- [ ] A captured ACP request serializes with exactly `child_id`, `parent_id`,
  `placement`, `acp_profile`, `instructions`, `task`, `budget`, and `capability`;
  cancellation, activity sink, and the live input queue arrive only as separate
  `start_child` arguments.
- [ ] ACP placement still constructs no native VFS, `AgentCell`, provider, tool
  registry, or sandbox environment.
- [ ] Unknown placement and missing ACP runtime errors occur synchronously before
  a running acknowledgement.
- [ ] Effective child capabilities remain placement ∩ parent ∩ optional caller
  attenuation, and instructions cannot widen them.
- [ ] Descendant spawning is controlled only by effective
  `spawn_placements`/`allowed_child_placements` capability.
- [ ] Native skill availability retains S017 placement-configured discovery and
  capability intersection; `instructions` do not grant unavailable skills.
- [ ] With `max_sub_agents = 1`, 32 concurrent authorized calls produce exactly
  one acknowledgement, one factory invocation, and one `SubAgentSpawned` entry;
  all other calls fail reservation.
- [ ] Concurrent unknown or unauthorized placement calls produce no factory
  invocation or journal entry and do not change `used_sub_agents`; empty tool or
  token placement lists deny every call.
- [ ] Before its actor starts or a direct operation is exposed, every production
  supervisor is explicitly bound to its configured root id. An unbound direct
  spawn and every unbound parent-facing actor spawn/control payload fail closed
  with zero factory, hook, budget, journal, activity, task, or supervisor-map
  effects; sending an arbitrary first caller does not bind or authorize it.
- [ ] A host-internal duplicate child id is rejected synchronously by both
  direct and actor spawn paths before any factory method or accepted-spawn
  effect. The original running or terminal child remains unchanged and usable,
  a closed id remains closed, and no model-visible schema permits supplying an
  id.

### Lifecycle and observability

- [ ] Spawn acknowledgements and child status/list/wait/join results contain
  `child_id`, `placement`, and status metadata, with no `agent_type` field.
- [ ] Across 1,000 concurrently accepted spawns in one root session, every id is
  distinct and matches `^child-[0-9a-f]{32}$`; fixture placement, task, and
  instruction substrings occur in none of the ids.
- [ ] Child activity events and engine projections use `placement`/
  `child_placement`, with no `agent_type`/`child_agent_type` field.
- [ ] A three-level activity fixture reaches the root with one recursively
  nested `ChildActivity` wrapper for each immediate-parent hop, preserving each
  hop's exact child id and placement and never flattening the grandchild into a
  direct root child.
- [ ] `JOURNAL_SCHEMA_VERSION` is 3; all new entries use version 3, and every
  in-memory and SQLite append/read/query/fork/replay path rejects version 2 or 4
  with `SchemaVersionMismatch { expected: 3, got }` before decoding, counting,
  copying, or replaying an entry.
- [ ] The version-3 `SubAgentSpawned` entry has exactly child id, placement,
  backend, task, and effective instructions, and is durable before the fake
  child runtime observes execution.
- [ ] Spawn before-hooks receive the exact context in this spec before any
  reservation or accepted-spawn effect; deny/kill leaves no accepted child,
  reservation, acknowledgement, or spawn entry.
- [ ] Caller-omitted capabilities appear in the before-hook context as the full
  pre-chain effective token, never `null`, and all hook capability output is
  checked as a subset of that token.
- [ ] Spawn hook changes to placement, backend, instructions, task, or unknown
  fields fail closed; budget/capability narrowing is revalidated, reaches the
  child factory/runtime, and widening fails before acceptance.
- [ ] Spawn after-hooks receive the exact context in this spec in reverse order,
  run before terminal caching and `SubAgentCompleted`, and their modified JSON
  does not alter the typed cached terminal result.
- [ ] Spawn-hook deny/kill entries are written to the parent journal; a
  before-hook deny leaves the parent running, while before- or after-hook kill
  terminates the spawning parent with `PolicyKill` and an after-hook kill still
  preserves the child's original cached/journaled terminal result. If the
  `HookKill` append fails, the failure is reported at `ERROR` in logs and on the
  active span, but `PolicyKill` remains authoritative; before-kill still has
  zero accepted effects and after-kill still preserves child settlement.
- [ ] Every `ExitReason` follows the exhaustive child-settlement table:
  complete/max-turns/budget-exhausted are completed successes;
  error/guardrail/policy-kill are failed non-successes; cancellation is a
  cancelled non-success; and awaiting-approval remains nonterminal without
  after-hooks, `ChildFinished`, terminal caching/delivery, or
  `SubAgentCompleted` until the same child resumes to a terminal exit.
- [ ] Interactive tracking retains all concurrently accepted opaque child ids;
  each `ChildFinished` removes only its exact id. Ctrl-C during a single-child
  join cancels only that call's explicit selected id, waits for the real
  `CancelChild` acknowledgement before rendering cancellation-requested output,
  and never fabricates success or chooses another child when acknowledgement
  fails or no child is explicitly selected.
- [ ] The `create_agent` span carries `simulacra.child.placement` and
  `simulacra.child.backend`; raw task/instruction/skill text is absent from
  metric labels and logs record only instruction length.
- [ ] Provider, ACP-port, and other runtime child-execution failures log `WARN`
  with bounded `error_category` values `provider`, `acp_runtime`, and `runtime`,
  respectively. Captured logs contain no raw failure text, task or instruction
  text, or skill name, and no failure category is added as a metric label.
- [ ] TraceQL confirms native and ACP `create_agent` spans carry the correct
  placement/backend attributes and remain nested under the parent trace.
