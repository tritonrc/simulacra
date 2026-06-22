# simulacra-provider

Adapters that implement the `Provider` trait (from `simulacra-types`) for
concrete LLM backends.

## Current backends

- **AnthropicProvider** (behind the `anthropic` feature flag, default-enabled)

## Dependencies

- `simulacra-types` — trait definitions, message types, budget, errors
- `reqwest` — HTTP client for API calls
- `serde` / `serde_json` — request/response serialization

## How to test

```bash
cargo test -p simulacra-provider
cargo clippy -p simulacra-provider -- -D warnings
```

Note: the `chat()` implementation is currently `todo!()`.
Integration tests will require a live API key once implemented.
