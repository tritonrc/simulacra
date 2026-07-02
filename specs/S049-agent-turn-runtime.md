# S049 — Agent Turn Runtime Foundation

**Status:** Active
**Crate:** `simulacra-runtime`

## Context

S015 and S034 require the runtime to support cancellation, tool approval, and
future streaming/HITL flows, but the current `AgentLoop` is still a compact
whole-response ReAct loop. S049 introduces the runtime structure needed for
those follow-up features without changing the provider wire contract or server
API behavior.

The design stays Simulacra-native: side effects remain mediated by the host,
tool side effects still go through `AgentCell`, and replay remains
deterministic.

## Behavior

1. The runtime exposes turn primitives:
   - `ActiveTurn` — owns mutable state for the running turn.
   - `TurnState` — records tool call count and terminal cancellation state.
   - `TurnContext` — immutable per-turn identifiers, model, capability, and
     cancellation state.
   - `StepContext` — exact model-visible snapshot for one provider call:
     compacted messages plus direct tool definitions.
2. `AgentLoop` captures a `StepContext` before every provider call and uses
   that snapshot for the provider request. Later registry changes must not
   affect an in-flight provider request.
3. Tool execution is delegated to a `ToolCallRuntime`.
4. The runtime honors per-tool `supports_parallel_tool_calls` metadata:
   - default `false` tools run serially in provider order;
   - all-parallel tool batches may execute concurrently;
   - final tool result messages are appended in provider order.
5. Replay disables parallel tool execution. Recorded journal order is consumed
   deterministically.
6. The runtime honors `waits_for_runtime_cancellation` metadata:
   - non-waiting tools may be aborted when runtime cancellation is observed;
   - waiting tools are allowed to finish cleanup before the cancelled tool
     response is returned.
7. Cancellation before a provider call exits with `ExitReason::Cancelled`
   without invoking the provider.
8. Cancellation during tool dispatch returns an error tool result with
   `"cancelled by user"` content. The tool result is journaled before the tool
   message is appended.
9. `Provider::chat` remains unchanged. Streaming provider events are handled by
   S050. Tool-call input deltas, `WaitingApproval` resume, and `input.response`
   consumption are deferred to later specs.

## Assertions

- [x] `StepContext` freezes direct tool definitions and compacted messages used
  for a provider call. Behavioral test:
  `agent_loop_uses_step_context_for_provider_input`.
- [x] Existing `AgentLoop::run` and `run_single_turn` callers continue to work.
  Behavioral coverage: existing agent-loop tests plus S049 cancellation tests.
- [x] Serial tools execute and journal in provider order. Behavioral coverage:
  existing `tool_call_then_text_response` and journal-ordering tests.
- [x] A batch where every tool supports parallel calls may overlap execution.
  Behavioral test:
  `all_parallel_tool_batches_overlap_and_preserve_provider_order`.
- [x] Parallel tool result messages are appended in provider order. Behavioral
  test: `all_parallel_tool_batches_overlap_and_preserve_provider_order`.
- [x] Replay forces serial deterministic tool result consumption. Behavioral
  test:
  `replay_tool_batches_use_recorded_serial_results_even_when_tools_are_parallel_capable`.
- [x] Cancellation before provider call returns `ExitReason::Cancelled` and
  does not call the provider. Behavioral test:
  `cancellation_before_provider_returns_cancelled_without_provider_call`.
- [x] Cancellation during a non-waiting tool returns a cancelled error result.
  Behavioral test:
  `cancellation_during_non_waiting_tool_returns_cancelled_error_result`.
- [x] Cancellation during a waiting tool waits for cleanup and then returns a
  cancelled error result. Behavioral test:
  `cancellation_during_waiting_tool_waits_for_cleanup_before_cancelled_result`.
- [x] Tool call and tool result journal entries are written before the
  corresponding tool result message is appended. Behavioral coverage: existing
  journal-before-return tests and S049 tool cancellation tests.

## Out of Scope

- Provider streaming trait redesign beyond the S050 companion streaming
  contract.
- Incremental token, reasoning, or tool-argument deltas from providers.
- Server-side `input.response` / `approval.respond` resume behavior.
- Hosted tools, host process execution, approval escalation, or Codex
  compatibility behavior.
