# S065 — Anthropic Automatic Prompt Caching

**Status:** Active — implemented
**Crates involved:** `simulacra-provider`

## Dependencies

- **S059** — cache usage is parsed and reported; this spec makes it non-zero

## Why this spec exists

An agent loop resends its whole history on every call. Without a cache
breakpoint, Anthropic bills every call's full input at the base rate. A host
measured a coordinator sending 31k input tokens to answer "6 × 7", and 163k
across two calls of one turn, with nothing cached.

## Contract

- `AnthropicProvider::with_prompt_caching()` opts a provider in. It is off by
  default, so existing hosts send byte-identical requests.
- When on, every request carries the top-level field
  `"cache_control": {"type": "ephemeral"}`. Anthropic places the breakpoint on
  the last cacheable block. A later call reads that prefix from cache only
  when all of these hold: the prefix is unchanged, it meets the model's
  minimum cacheable length, the entry is younger than its five-minute TTL,
  and it lies within Anthropic's 20-block lookback of the new breakpoint.
  Otherwise the prefix is written again at the cache-write rate (1.25x input);
  below the minimum length nothing is cached and nothing extra is billed.
- Both the synchronous and the streaming paths send it.
- The default five-minute TTL applies. No explicit block-level breakpoints are
  added.

## Assertions

- [x] With caching on, the request body of both `chat` and `chat_stream`
  carries `cache_control: {"type": "ephemeral"}`.
- [x] With caching off, neither request body carries `cache_control`.

## Out of Scope

- Block-level breakpoints, the one-hour TTL, and prefix reordering.
- Caching on the OpenAI-compatible provider, where caching is the gateway's
  own behavior.
