# simulacra-context

Strategies for compacting conversation history to fit within a
provider's context window.

## Key types

- **SlidingWindowStrategy** — keeps the system message plus as many
  recent messages as fit within the token limit. Uses a stub token
  counter (4 chars ~= 1 token) until a real tokenizer is wired in.

## Dependencies

- `simulacra-types` — `ContextStrategy` trait, `Message`, `Role`

## How to test

```bash
cargo test -p simulacra-context
cargo clippy -p simulacra-context -- -D warnings
```

Unit tests cover basic sliding-window behavior and edge cases.
