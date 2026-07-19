# S054 — Child Agent Orchestration

**Status:** Active
**Crates involved:** `simulacra-cli`, `simulacra-runtime`, `simulacra-server`

## Dependencies

- **ARCHITECTURE.md** — supervision model, actor priority ordering, Golden Rule
- **S009** — Agent supervisor lifecycle and message priority
- **S018** — child spawn, join, cancel, and steer tools
- **S023** — generic sub-agents remain leaf workers

## Scope

Add lightweight orchestration tools on top of the S018 child-agent contract:
`child_status`, `wait_child_agent`, and `close_child_agent`.

These tools let parent agents inspect live handles, wait with bounded timeouts,
and explicitly release completed child state. They do not change the existing
`spawn_agent`, `join_child_agent`, `cancel_child_agent`, or `steer_child_agent`
semantics.

## Behavior

### Model-visible tool guidance

- [x] Child-control tool descriptions are model-visible behavioral guidance.
  They are part of the tool contract sent to providers when the tools are
  available, not incidental implementation prose.
- [x] `spawn_agent` description explains the intended lifecycle: spawn only
  concrete, bounded, independent subtasks; do not delegate immediate
  critical-path blockers; the tool returns a live handle, not a final answer;
  continue non-overlapping parent work after spawning; use `child_status` for
  cheap inspection; use `wait_child_agent` for bounded polling or wait-any;
  use `join_child_agent` when the terminal result is needed; and use
  `close_child_agent` only for terminal cleanup.
- [x] `child_status` description identifies it as a cheap nonblocking
  handle/status probe and states that terminal status variants contain the
  child's result.
- [x] `wait_child_agent` description identifies it as a bounded,
  non-consuming wait; states that `timeout_ms = 0` is a poll; states that
  `child_ids` performs wait-any; and states that a running timeout is a
  successful non-error result.
- [x] `join_child_agent` description identifies it as the canonical,
  potentially-blocking terminal-summary API.
- [x] `close_child_agent` description identifies it as terminal cleanup only,
  not cancellation.
- [x] These description changes do not change supervisor behavior, tool names,
  input schemas, registry exposure, child lifecycle semantics, or provider
  `ToolDefinition` shape.

### Supervisor state

- [x] The supervisor tracks stable child metadata for each accepted child:
  `child_id`, `agent_type`, original `task`, `parent_id`, `started_at_ms`,
  optional `finished_at_ms`, and terminal result state.
- [x] `ChildStatus` and `ChildRosterEntry` share a public `ChildAgentStatus`
  sum type with wire variants `"running"`, `{ "completed": string|null }`,
  `{ "failed": string|null }`, and `{ "cancelled": string|null }`.
- [x] Running children report status `"running"` and `ready: false`.
- [x] Successful terminal children report
  `{ "completed": <final assistant message> }` and `ready: true`.
- [x] A successful child with no final assistant message reports
  `{ "completed": null }`; an authored empty final assistant message reports
  `{ "completed": "" }`.
- [x] Failed and cancelled terminal children report `{ "failed": <error> }`
  and `{ "cancelled": <error> }`, respectively, with `ready: true`; absent
  terminal content is represented by `null`.
- [x] Status and roster probes clone cached terminal results and never consume
  them. Repeated probes return the same content, and a later
  `join_child_agent` returns a summary from the same cached terminal result.
- [x] Signal-priority cancellation is processed ahead of command-priority
  status, wait, and close requests when messages are queued together.

### Terminal result delivery tracking

- [x] Each accepted child tracks a private, monotonic `result_delivered` flag
  alongside its cached terminal result. The flag starts `false`, changes only
  from `false` to `true`, and does not consume or alter the cached result.
- [x] `result_delivered` means that a terminal outcome was successfully returned
  through a parent-facing result-bearing supervisor operation. It does not
  claim that the parent model reasoned about, narrated, or otherwise acted on
  the result.
- [x] Successfully returning terminal content through `child_status`,
  `list_child_agents`, single-child or wait-any `wait_child_agent`, or
  `join_child_agent` sets `result_delivered: true`. This includes completed,
  failed, and cancelled outcomes, absent content represented by `null`, and an
  authored empty string.
- [x] Running status/list entries, running wait results, failed response-channel
  sends, host-only inspection, and `close_child_agent` do not set
  `result_delivered`.
- [x] A successful `list_child_agents` response marks every terminal child whose
  body it contains and does not mark running children. A wait-any response marks
  only the terminal child selected for that response.
- [x] Delivery is linearized with terminal-result access: once a receiver
  observes a successful terminal response, host inspection already reports
  `result_delivered: true`; concurrent result-bearing operations cannot regress
  the flag or alter the cached result; and a failed response-channel send leaves
  a later successful delivery able to make the monotonic transition.
- [x] A public host-only `ChildResultInspection` contains the cached
  `ChildTerminalResult` and the current `result_delivered` value.
  `SupervisorPayload::InspectChildResult` returns this inspection without
  changing delivery state and is not registered as a model-visible tool.
- [x] Repeated host inspection is stable and non-mutating. It reports `false`
  until a parent-facing operation successfully returns terminal content and
  reports `true` afterward, until explicit close removes the child.
- [x] The existing model-visible JSON shapes for `child_status`,
  `list_child_agents`, `wait_child_agent`, and `join_child_agent` remain
  unchanged; `result_delivered` is exposed on the host inspection result only.

### `child_status`

- [x] `child_status` accepts `{ "child_id": string }`.
- [x] Empty or missing `child_id` returns `ToolError::InvalidArguments`.
- [x] A known child returns `{ "child_id", "agent_type", "status", "ready", "elapsed_ms" }`.
- [x] Unknown or closed children return an error tool result.
- [x] `child_status` sends `SupervisorPayload::ChildStatus` with
  `MessagePriority::Command`.
- [x] A closed supervisor channel returns an error tool result.

### `wait_child_agent`

- [x] `wait_child_agent` accepts `{ "child_id": string, "timeout_ms": integer }`
  for a single child.
- [x] `wait_child_agent` accepts `{ "child_ids": string[], "timeout_ms": integer }`
  to wait for any listed child to become terminal.
- [x] Empty or missing `child_id` returns `ToolError::InvalidArguments`.
- [x] Empty, missing, or non-string `child_ids` entries return
  `ToolError::InvalidArguments`.
- [x] Providing both `child_id` and `child_ids` returns
  `ToolError::InvalidArguments`.
- [x] Missing, negative, or non-integer `timeout_ms` returns `ToolError::InvalidArguments`.
- [x] `timeout_ms = 0` polls once without waiting.
- [x] If the child is still running after the timeout, the tool returns
  `{ "child_id", "status": "running", "ready": false }` as a non-error result.
- [x] If all listed children are still running after the timeout, the tool
  returns `{ "child_ids", "status": "running", "ready": false }` as a non-error
  result.
- [x] If the child is terminal, the tool returns `{ "child_id", "status",
  "ready": true, "agent_type", "exit_reason", "message", "token_usage" }`,
  matching the `join_child_agent` terminal summary where applicable.
- [x] If any listed child is terminal, the tool returns that child's terminal
  summary shape with `child_id`, `status`, `ready`, `agent_type`,
  `exit_reason`, `message`, and `token_usage`.
- [x] If multiple listed children are already terminal when polled, the first
  terminal child in `child_ids` order is returned.
- [x] Waiting does not consume the terminal result; `join_child_agent` can still
  return the same terminal result later.
- [x] `wait_child_agent` sends `SupervisorPayload::WaitChild` with
  `MessagePriority::Command`.
- [x] Multi-child `wait_child_agent` sends `SupervisorPayload::WaitChildren`
  with `MessagePriority::Command`.
- [x] A closed supervisor channel returns an error tool result.

### `close_child_agent`

- [x] `close_child_agent` accepts `{ "child_id": string }`.
- [x] Empty or missing `child_id` returns `ToolError::InvalidArguments`.
- [x] Running children are rejected with an error; close is not cancellation.
- [x] Terminal children are removed from supervisor result/status maps and
  return `{ "child_id", "status": "closed" }`.
- [x] After close, `child_status`, `wait_child_agent`, `join_child_agent`,
  `steer_child_agent`, and `cancel_child_agent` return an unknown-or-closed
  error for that child.
- [x] `close_child_agent` sends `SupervisorPayload::CloseChild` with
  `MessagePriority::Command`.
- [x] A closed supervisor channel returns an error tool result.

### Terminal child summaries

- [x] `join_child_agent` terminal JSON includes structured `status`,
  `exit_reason`, `message`, `token_usage`, `elapsed_ms`, `tool_uses`,
  `artifacts`, and `vfs_changes` fields.
- [x] `wait_child_agent` terminal JSON includes the same structured summary
  fields as `join_child_agent` when a single child or wait-any child is
  terminal.
- [x] `status` is one of `"completed"`, `"failed"`, or `"cancelled"`.
- [x] `tool_uses` is derived from structured child output, not parent-side
  prose parsing.
- [x] `artifacts` and `vfs_changes` are structured arrays and are empty until
  child artifact/change tracking is introduced by a later spec.

### Registry

- [x] CLI root registries that include S018 child-control tools also include
  `child_status`, `wait_child_agent`, and `close_child_agent`.
- [x] Server worker root registries that include S018 child-control tools also
  include `child_status`, `wait_child_agent`, and `close_child_agent`.
- [x] Spawn-capable configured child registries include `spawn_agent`,
  `join_child_agent`, `cancel_child_agent`, `steer_child_agent`,
  `child_status`, `wait_child_agent`, and `close_child_agent`.
- [x] Generic leaf child registries still exclude all child-control tools.

### `list_child_agents`

- [x] `list_child_agents` preserves `child_id`, `agent_type`, `task`, `ready`,
  and `elapsed_ms`, and uses the shared `ChildAgentStatus` wire contract for
  `status`.
- [x] Mixed running and terminal rosters are sorted deterministically by
  `child_id` and include cached terminal content without consuming it.
- [x] `list_child_agents` description states that terminal status variants
  contain each child's result.
- [x] Closing a terminal child removes it from subsequent status and roster
  probes without changing close semantics.

## Non-Goals

- No interrupt steering.
- No turn-id validation.
- No context handoff or artifact exchange.
- `join_child_agent` remains the canonical indefinite terminal-result API.
- `wait_child_agent` is a non-consuming orchestration probe.
- `close_child_agent` is explicit cleanup only; it never cancels or aborts live work.
