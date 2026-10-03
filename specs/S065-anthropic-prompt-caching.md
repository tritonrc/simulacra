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
  `prompt_caching()` reports the setting, so a host can test its own wiring.
- When on, every request carries the top-level field
  `"cache_control": {"type": "ephemeral"}`. Anthropic places the breakpoint on
  the last cacheable block. A later call reads that prefix from cache only
  when all of these hold: the prefix is unchanged, it meets the model's
  minimum cacheable length, the entry is younger than its five-minute TTL,
  and it lies within Anthropic's 20-block lookback of the new breakpoint. A
  read is billed at the model's cache-read rate, a fraction of its input rate
  that varies by model (0.1x for most).
  Otherwise the prefix is written again at the cache-write rate (1.25x input);
  below the minimum length nothing is cached and nothing extra is billed.
- Both the synchronous and the streaming paths send it.
- The default five-minute TTL applies.
- A host that places content changing every wake after a stable transcript
  marks that content's message with `cache_breakpoint_before()`, an
  `anthropic` provider-content block. With caching on, the request also puts
  `cache_control` on the last cacheable block of the API message before it
  (thinking blocks and empty text cannot carry it), so the next wake can read
  the transcript back. At most three marked prefixes are honoured, the last
  three; automatic caching takes the fourth breakpoint. A marked system
  message is ignored, since system content leads every request. With caching
  off the marker adds nothing, and other providers ignore it.

## Assertions

- [x] With caching on, the request body of both `chat` and `chat_stream`
  carries `cache_control: {"type": "ephemeral"}`.
- [x] With caching off, neither request body carries `cache_control`.
- [x] `prompt_caching()` is false by default and true after opting in.
- [x] A marked message puts `cache_control` on the last cacheable block of the
  message before it; a string content becomes one text block; thinking and
  empty text are skipped; only the last three marks apply.
- [x] With caching off, a marked message adds no `cache_control`.
- [x] A marked system message adds no breakpoint.

## Out of Scope

- The one-hour TTL, and reordering a host's messages.
- Caching on the OpenAI-compatible provider, where caching is the gateway's
  own behavior.
