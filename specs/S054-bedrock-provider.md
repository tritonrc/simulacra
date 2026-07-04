# S054 — Bedrock Provider

**Status:** Active
**Crate:** `simulacra-provider`
**Depends on:** S007 (Provider trait), S010 (OTel GenAI conventions)

## Scope

Add an Amazon Bedrock provider (`BedrockProvider`) that implements the
`Provider` and `StreamingProvider` traits from `simulacra-types`. It targets
the Bedrock **Converse API** (`/model/{modelId}/converse` and
`/converse-stream`), which is model-agnostic and AWS-recommended.

AWS request signing (SigV4) is implemented in-process — no AWS SDK
dependency. This preserves the single-binary / minimal-dependency
philosophy from `ARCHITECTURE.md`.

## Behavior

1. `BedrockProvider` implements `Provider::chat` and `StreamingProvider::chat_stream`.
2. Requests are authenticated with **AWS Signature Version 4**, computed
   in-process from `access_key_id`, `secret_access_key`, optional session
   `token`, `region`, and `service = "bedrock"`.
3. The provider endpoint is `https://bedrock-runtime.{region}.amazonaws.com`.
   - Non-streaming: `POST /model/{modelId}/converse`
   - Streaming: `POST /model/{modelId}/converse-stream`
   - `modelId` is URL path-encoded.
4. Budget is checked **before** signing or sending (S007 §3). If the budget
   is exhausted, return `ProviderError::BudgetExhausted` with no HTTP call.
5. `max_tokens` is derived from the remaining token budget (`0` budget =
   unlimited → default cap) and sent as `inferenceConfig.maxTokens`.
6. Provider errors map to the existing `ProviderError` taxonomy:
   - HTTP 400 → `BadRequest`
   - HTTP 401/403 → `AuthError`
   - HTTP 429 → `RateLimit` (retryable)
   - HTTP 529 → `Overloaded` (retryable)
   - HTTP 500–599 → `ServerError` (retryable)
7. The provider does **not** mutate the budget — it returns `TokenUsage` and
   the caller is responsible for accounting (S007 §"Provider does not
   mutate budget").
8. Bedrock error bodies (`{"message": "..."}`) are parsed for the human-readable message.
9. `infer_provider_kind` recognizes a `bedrock:` model prefix as the Bedrock
   provider. (Native model ids such as `anthropic.claude-...` are also
   accepted when the prefix form is used, e.g. `bedrock:anthropic.claude-3-5-sonnet-...`.)

### SigV4 signing

- Canonical request, string-to-sign, derived signing key, and signature are
  computed per the AWS SigV4 specification for SigV4 (HMAC-SHA256).
- The signed request adds `Authorization` and `x-amz-date` headers.
- When a session token is present, `x-amz-security-token` is added to both
  the signed headers and the outgoing request.
- The payload hash (`x-amz-content-sha256`) is the hex SHA-256 of the body
  and is part of the signed canonical request.

### Converse API mapping

- System messages → top-level `system` array of `{text: ...}`.
- User/assistant messages → `messages` array with `role` + `content` blocks.
  - Text content → `{text: ...}`.
  - Assistant tool calls → `{toolUse: {toolUseId, name, input}}`.
  - Tool results (role `tool`) → user message with `{toolResult: {toolUseId, content}}`.
- Tools → `toolConfig.tools` array of `{toolSpec: {name, description, inputSchema.json}}`.
- Response `output.message.content` blocks → `ProviderResponse.message`:
  `{text}` → content; `{toolUse}` → `ToolCallMessage`.
- `stopReason` mapping: `tool_use` → `ToolUse`, `max_tokens` → `MaxTokens`,
  `stop_sequence` → `StopSequence`, otherwise `EndTurn`.
- `usage.inputTokens` / `usage.outputTokens` → `TokenUsage`.

### Converse-stream mapping

The SSE event stream is parsed incrementally. Recognized events:
- `messageStart` → captures response id.
- `contentBlockStart` with `start.toolUse` → begins a tool call block.
- `contentBlockDelta` with `delta.text` → text delta (emitted to sink);
  with `delta.toolUse` → accumulates tool-use `input` JSON fragments
  (emitted to sink as `ToolCallDelta`).
- `contentBlockStop` → finalizes a tool call block (parses accumulated JSON).
- `messageDelta` → `stopReason`.
- `metadata` → `usage`.
- `messageStop` → end.

## Assertions

### Budget & errors
- [x] Budget exhausted returns `ProviderError::BudgetExhausted` with no HTTP call (fake HTTP panics if invoked).
- [x] Successful text response maps content, usage, finish reason, response id, and model into `ProviderResponse`.
- [x] Tool-use response maps `toolUse` content blocks into `ToolCallMessage`s with parsed JSON arguments.
- [x] HTTP 400 maps to non-retryable `BadRequest`.
- [x] HTTP 401 maps to non-retryable `AuthError`.
- [x] HTTP 429 maps to retryable `RateLimit`.
- [x] HTTP 500 maps to retryable `ServerError`.
- [x] Bedrock error body `{"message": "..."}` is surfaced in the error string.

### SigV4
- [x] Signing the AWS SigV4 "get-vanilla" IAM reference vector reproduces the documented signature.
- [x] Signing produces an `Authorization` header with `AWS4-HMAC-SHA256` scheme and the credential scope.
- [x] A session token adds `x-amz-security-token` to the signed and outgoing headers.
- [x] The outgoing request carries `x-amz-date` and `x-amz-content-sha256` headers.

### Converse request shape
- [x] Request body has `modelId`, `messages`, `inferenceConfig.maxTokens`, and (when tools present) `toolConfig.tools`.
- [x] System messages are lifted into the top-level `system` array, not duplicated in `messages`.
- [x] The model id is URL path-encoded in the request target.

### Object safety & budget immutability
- [x] `BedrockProvider` is usable as `Box<dyn Provider>`.
- [x] The provider does not mutate the caller's budget.

### Streaming
- [x] `chat_stream` emits `TextDelta` events in arrival order and assembles a final `ProviderResponse`.
- [x] `chat_stream` emits `ToolCallDelta` events and assembles a final tool-call `ProviderResponse` from `converse-stream`.

### Selection
- [x] `ProviderKind::Bedrock` is wired through runtime, server, and CLI provider construction, inferable from a `bedrock:` model prefix.

## Observability (see S010)

- [x] Every chat call produces a span named `chat {model}` with `gen_ai.*` attributes and `gen_ai.provider.name = "bedrock"`.
- [x] `gen_ai.usage.input_tokens` / `gen_ai.usage.output_tokens` are recorded after the response.
- [x] `gen_ai.client.token.usage` and `gen_ai.client.operation.duration` meters record observations tagged with the bedrock provider.
