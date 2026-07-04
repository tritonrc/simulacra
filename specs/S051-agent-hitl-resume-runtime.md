# S051 — Agent HITL Resume Runtime

**Status:** Active
**Crates involved:** `simulacra-types`, `simulacra-runtime`, `simulacra-server`

## Context

S031 and S034 define `input.response` and `approval.respond`, but the server
previously stopped at the command shape: `TaskManager::provide_input` returned
`NotImplemented`, and `AgentLoop` had no input channels to consume. S051 wires
human-in-the-loop continuation into the runtime while preserving deterministic
journal replay.

## Behavior

1. Server-launched tasks may opt into HITL through task metadata:
   - `enable_human_input: true` exposes the `request_input` tool.
   - `require_tool_approval: true` requires approval before non-input tools run.
2. `request_input` emits `ActivityEvent::InputRequired`, waits for
   `input.response`, and returns the response as a normal tool result.
3. Tool approval emits `ActivityEvent::ToolApprovalRequired` after the final
   provider response is assembled and before `ToolStart`.
4. Approved tool calls execute normally.
5. Denied tool calls do not execute and return an error tool result to the
   model.
6. `TaskManager::provide_input` and `TaskManager::respond_approval` send
   responses to live agent-loop channels and transition waiting tasks back to
   `Running`.
7. Wrong approval `tool_call_id` responses are rejected before they can unblock
   the waiting tool call.
8. Replay never waits on HITL channels; it consumes recorded `ToolResult`
   entries.
9. Cancellation while waiting for input or approval exits through the normal
   runtime cancellation path without executing the pending side effect.

## Assertions

- [x] `request_input` emits `InputRequired`, waits for `input.response`, and
  journals one final tool result. **Tested by
  `request_input_tool_waits_for_input_response_and_journals_tool_result`.**
- [x] Tool approval emits before `ToolStart`; approval executes the tool.
  **Tested by `tool_approval_required_emits_before_tool_start_and_approval_executes`.**
- [x] Denied approval does not execute the tool and returns an error tool
  result. **Tested by
  `tool_approval_denial_returns_error_result_without_executing_tool`.**
- [x] Replay consumes recorded tool results without waiting on HITL channels.
  **Tested by `replay_consumes_recorded_hitl_tool_result_without_waiting`.**
- [x] `input.response` transitions a waiting task back to `running` and sends
  the response to the live channel. **Tested by
  `input_response_sends_to_live_channel_and_transitions_to_running`.**
- [x] `approval.respond` approve/deny transitions a waiting task back to
  `running` and sends the decision. **Tested by
  `approval_response_sends_to_live_channel_and_transitions_to_running`.**
- [x] Wrong approval `tool_call_id` is rejected without unblocking the waiting
  call. **Tested by
  `approval_response_rejects_mismatched_tool_call_id_without_sending`.**
- [x] Server event bridging maps `InputRequired` and `ToolApprovalRequired` to
  `input.required` and `tool.approval_required`. **Tested by
  `hitl_activity_events_translate_to_server_events_and_waiting_states`.**

## Out of Scope

- CLI interactive approval rewrite; existing CLI approval behavior remains
  separate.
- Durable pause/resume across process restart.
- Persistent shell or provider stream sessions.
