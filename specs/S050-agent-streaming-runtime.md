# S050 — Agent Streaming Runtime

**Status:** Active
**Crates involved:** `simulacra-types`, `simulacra-provider`, `simulacra-runtime`, `simulacra-server`, `simulacra-frontend`

## Context

S049 split the agent turn into explicit runtime phases but intentionally left
provider streaming deferred. S050 connects provider SSE streams to the activity
event stream without changing the stable `Provider::chat` fallback path or the
journal replay model.

The runtime remains deterministic: provider deltas are display-only activity
events, and the journal records one final assembled `LlmResponse` after a
successful provider call.

## Behavior

1. Provider streaming is exposed through a companion streaming contract; the
   existing `Provider::chat` method remains unchanged for non-streaming callers.
2. Streaming-capable providers emit provider stream events as the upstream
   response arrives and still return a final assembled `ProviderResponse`.
3. `AgentLoop` uses the streaming provider path when available during live
   execution.
4. `AgentLoop` emits `ActivityEvent::Token` for each provider text delta in
   provider order.
5. Provider thinking events map to `ThinkStart`, `ThinkDelta`, and `ThinkEnd`.
   Runtime measures thinking duration and estimates thinking tokens as
   accumulated thinking characters divided by four.
6. Streaming must not duplicate the final assistant text as an additional token
   event. The full-response token emission is only for non-streaming fallback
   and replay.
7. The runtime journals exactly one `LlmResponse` after a stream completes and
   before the assistant message is appended or tool calls are dispatched.
8. Replay does not call the streaming provider path. It consumes the recorded
   final response and may emit the same single full-response token event as the
   non-streaming fallback.
9. Cancellation during a provider stream exits the turn as cancelled, drops the
   provider future, does not journal `LlmResponse`, and does not append partial
   assistant text to conversation state.
10. OpenAI and Anthropic providers use their real HTTP streaming path for
    streaming-capable calls, while preserving buffered `chat` behavior.
11. Streaming tool-call argument chunks emit observational tool-call delta
    events as they arrive. These deltas are not journaled and do not mean the
    tool has started executing.
12. `ToolStart` remains the execution boundary and is emitted only after the
    final provider response contains a complete `ToolCallMessage`.
13. Tool-call deltas carry the provider tool-call `index`, optional
    `tool_call_id`, optional `name`, and an `arguments_delta` string.
14. OpenAI emits tool-call deltas from `delta.tool_calls[*]` metadata and
    argument chunks. Anthropic emits tool-call deltas from `tool_use` block
    metadata and `input_json_delta.partial_json` chunks.

## Assertions

- [x] Provider streaming contract is object-safe and leaves `Provider::chat`
  unchanged. **Tested by `streaming_provider_contract_is_object_safe_and_optional`.**
- [x] Runtime streaming emits token deltas in provider order. **Tested by
  `streaming_provider_tokens_emit_in_order_and_final_response_is_journaled_once`.**
- [x] Runtime streaming assembles and journals one final assistant response.
  **Tested by
  `streaming_provider_tokens_emit_in_order_and_final_response_is_journaled_once`.**
- [x] Runtime streaming does not duplicate final text as a full-response token.
  **Tested by
  `streaming_provider_tokens_emit_in_order_and_final_response_is_journaled_once`.**
- [x] Non-streaming providers still use `Provider::chat` and emit one full token.
  **Tested by `non_streaming_provider_uses_chat_and_emits_single_full_token`.**
- [x] Replay does not call provider streaming and consumes the recorded response.
  **Tested by `replay_uses_recorded_response_without_streaming_provider_call`.**
- [x] Cancellation during stream returns cancelled and discards partial assistant
  text from messages and journal. **Tested by
  `cancellation_during_provider_stream_discards_partial_assistant_text`.**
- [x] OpenAI SSE text deltas are emitted in order while final response assembly
  remains correct. **Tested by
  `streaming_provider_emits_openai_text_deltas_and_assembles_final_response`.**
- [x] Anthropic SSE text deltas are emitted in order while final response
  assembly remains correct. **Tested by
  `streaming_provider_emits_anthropic_text_deltas_and_assembles_final_response`.**
- [x] OpenAI SSE tool-call argument deltas are emitted in stream order while
  final `ToolCallMessage` assembly remains correct. **Tested by
  `streaming_provider_emits_openai_tool_call_deltas_and_assembles_final_response`.**
- [x] Anthropic SSE tool-call argument deltas are emitted in stream order while
  final `ToolCallMessage` assembly remains correct. **Tested by
  `streaming_provider_emits_anthropic_tool_call_deltas_and_assembles_final_response`.**
- [x] Runtime maps provider tool-call deltas to `ActivityEvent::ToolCallDelta`
  without journaling partial tool-call state. **Tested by
  `provider_tool_call_deltas_map_to_activity_events_without_partial_journal_entries`.**
- [x] Replay does not emit tool-call deltas because replay consumes recorded
  final responses only. **Tested by `replay_does_not_emit_tool_call_deltas`.**
- [x] Cancellation during a provider stream may expose already-emitted deltas
  but does not journal or append partial tool-call state. **Tested by
  `cancellation_after_tool_call_delta_does_not_journal_or_append_partial_tool_state`.**
- [x] Server SSE maps runtime tool-call deltas to `tool.call_delta`. **Tested by
  `tool_call_delta_events_translate_to_tool_call_delta_with_optional_metadata_and_seq`.**
- [x] Frontend task streams merge `tool.call_delta` events into the matching
  in-progress tool block and replace them with the final `tool.called` block.
  **Tested by `agent-run.test.mjs`.**

## Out of Scope

- Persistent provider stream sessions or `write_stdin`-style interaction.
