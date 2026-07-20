# simulacra-provider

Adapters that implement the `Provider` trait (from `simulacra-types`) for
concrete LLM backends.

## Current backends

- **AnthropicProvider** (behind the `anthropic` feature flag, default-enabled)
- **OpenAiProvider** (behind the `openai` feature flag, default-enabled)
- **BedrockProvider** (behind the `bedrock` feature flag, default-enabled) —
  AWS Bedrock Converse API (`/converse`, `/converse-stream`) with in-process
  SigV4 signing (no AWS SDK dependency). See `specs/S054-bedrock-provider.md`.
  Credentials come from the standard `AWS_ACCESS_KEY_ID` /
  `AWS_SECRET_ACCESS_KEY` / optional `AWS_SESSION_TOKEN` env vars; region from
  `AWS_REGION` / `AWS_DEFAULT_REGION`. Select with a `bedrock:<modelId>` model
  string.

## Dependencies

- `simulacra-types` — trait definitions, message types, budget, errors
- `reqwest` — HTTP client for API calls
- `serde` / `serde_json` — request/response serialization
- (bedrock only) `sha2`, `hmac`, `hex`, `percent-encoding` — SigV4 signing +
  URL path encoding. All are already in the workspace dependency tree via
  `reqwest`/`rustls`, so no new transitive crates are pulled in.

## How to test

```bash
cargo test -p simulacra-provider
cargo clippy -p simulacra-provider -- -D warnings
```

The SigV4 signer is unit-tested against the official AWS SigV4 test suite
(`get-vanilla-empty-query-key`), and the converse-stream decoder against
hand-built binary event-stream frames, so no live AWS credentials are needed.
