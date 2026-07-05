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

### Supervisor state

- [x] The supervisor tracks stable child metadata for each accepted child:
  `child_id`, `agent_type`, original `task`, `parent_id`, `started_at_ms`,
  optional `finished_at_ms`, and terminal result state.
- [x] Running children report status `"running"` and `ready: false`.
- [x] Successful terminal children report status `"completed"` and `ready: true`.
- [x] Failed terminal children report status `"failed"` and `ready: true`.
- [x] Cancelled terminal children report status `"cancelled"` and `ready: true`.
- [x] Signal-priority cancellation is processed ahead of command-priority
  status, wait, and close requests when messages are queued together.

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

### Registry

- [x] CLI root registries that include S018 child-control tools also include
  `child_status`, `wait_child_agent`, and `close_child_agent`.
- [x] Server worker root registries that include S018 child-control tools also
  include `child_status`, `wait_child_agent`, and `close_child_agent`.
- [x] Spawn-capable configured child registries include `spawn_agent`,
  `join_child_agent`, `cancel_child_agent`, `steer_child_agent`,
  `child_status`, `wait_child_agent`, and `close_child_agent`.
- [x] Generic leaf child registries still exclude all child-control tools.

## Non-Goals

- No interrupt steering.
- No turn-id validation.
- No context handoff or artifact exchange.
- `join_child_agent` remains the canonical indefinite terminal-result API.
- `wait_child_agent` is a non-consuming orchestration probe.
- `close_child_agent` is explicit cleanup only; it never cancels or aborts live work.
