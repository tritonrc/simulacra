# S007 — Provider Trait

**Status:** Active
**Crate:** `simulacra-provider`

## Behavior

1. `Provider` trait: `async fn chat(&self, messages, tools, budget) -> Result<ProviderResponse>`.
2. Provider implementations handle HTTP, auth, serialization, and retry internally.
3. Budget is checked before making the API call. If budget is exhausted, return error without calling the API.
4. `ProviderResponse` includes `message`, `token_usage`, `finish_reason`, and `provider_response_id`.
5. Streaming responses are delivered through the S050 provider streaming contract; final `ProviderResponse` is assembled from the stream.
6. Provider errors are typed: `RateLimit`, `AuthError`, `BadRequest`, `ServerError`, `BudgetExhausted`.
7. Provider-native response blocks that are required for continuation (for example Anthropic `thinking` and `redacted_thinking` blocks) are preserved on `Message.provider_content`.
8. Anthropic signed thinking blocks keep their `signature`; redacted thinking blocks keep their encrypted `data`. Continued tool-use requests send those provider-native blocks back before the assistant tool-use block.

## Assertions

- [x] Provider checks budget before API call. **Tested in `budget_exhausted_returns_error_without_http_call` — fake HTTP client panics if called, proving no request was made.**
- [x] `ProviderResponse` includes token usage. **Tested in `successful_text_response_maps_correctly` and `provider_returns_usage_without_mutating_budget`.**
- [x] Rate limit error is retryable. **Tested in `rate_limit_429_is_retryable_with_retry_after`.**
- [x] Auth error is not retryable. **Tested in `auth_error_401_is_not_retryable`.**
- [x] Streaming responses are assembled into final ProviderResponse. **Tested in `streaming_event_stream_is_assembled_into_final_provider_response`. SSE events parsed and assembled into ProviderResponse.**
- [x] Streaming-capable providers emit text deltas while assembling the final response. **Covered by S050 tests `streaming_provider_emits_openai_text_deltas_and_assembles_final_response` and `streaming_provider_emits_anthropic_text_deltas_and_assembles_final_response`.**
- [x] Provider trait is object-safe (`Box<dyn Provider>`). **Tested in `provider_trait_is_object_safe` but not listed as spec assertion — adding.**
- [x] `ServerError` is retryable. **Tested via `Overloaded` (529) but no explicit 500 test.**
- [x] `BadRequest` is not retryable. **Tested in `bad_request_400_is_not_retryable`.**
- [x] Provider does not mutate budget — caller is responsible for updating usage. **Tested in `provider_returns_usage_without_mutating_budget`.**
- [x] Multiple provider backends can be selected by configuration (Anthropic, OpenAI, etc.). **Stub OpenAiProvider added. Tested in `crate_exposes_multiple_backends_for_configuration_selection`.**
- [x] Anthropic `thinking` and `redacted_thinking` response blocks are parsed into provider-native message content without becoming visible assistant text. **Tested in `thinking_response_blocks_do_not_break_text_mapping` and `thinking_response_blocks_do_not_break_tool_use_mapping`.**
- [x] Anthropic streaming preserves thinking text, `signature_delta`, and redacted thinking blocks on the final `ProviderResponse`. **Tested in `streaming_thinking_blocks_round_trip_on_final_response`.**
- [x] Anthropic tool-use continuation requests reserialize provider-native thinking blocks. **Tested in `build_request_parts_preserves_anthropic_thinking_blocks_on_assistant_tool_use` and `streaming_thinking_blocks_round_trip_on_final_response`.**
- [x] Runtime history, journaling, and replay resume preserve provider-native content across a tool-use round trip. **Tested in `provider_native_content_survives_tool_round_trip` and `replay_resume_preserves_provider_native_content_for_live_continuation`.**
- [x] Missing Anthropic thinking signatures are surfaced as a warning before request serialization. **Tested in `build_request_parts_warns_for_unsigned_anthropic_thinking_blocks`.**

## Observability (see S010 for conventions)

- [x] Every LLM call produces a span named `chat {model}` with all required `gen_ai.*` attributes from S010. **Tested in `chat_emits_span_with_required_gen_ai_attributes` and `chat_span_sets_otel_name_to_chat_and_model`.**
- [x] `gen_ai.usage.input_tokens` and `gen_ai.usage.output_tokens` are set on the span after response. **Tested in `token_counts_recorded_on_span` and `token_usage_attributes_are_recorded_as_numeric_fields`.**
- [x] `gen_ai.client.token.usage` histogram records token counts by operation and model. **Tested in `token_usage_histogram_is_recorded_with_operation_and_model_labels`.**
- [x] `gen_ai.client.operation.duration` histogram records call duration by operation and model. **Tested in `operation_duration_histogram_is_recorded_with_operation_and_model_labels`.**
- [x] `simulacra.tool.calls` counter is incremented per tool call with `tool_name` and `source`. **Tested in `tool_call_counter_increments_per_returned_tool_call_with_name_and_source`.**
- [x] Provider errors are logged at `WARN` (retryable) or `ERROR` (non-retryable) with error details. **Tested in `retryable_provider_errors_are_logged_at_warn_with_error_details` and `non_retryable_provider_errors_are_logged_at_error_with_error_details`.**
