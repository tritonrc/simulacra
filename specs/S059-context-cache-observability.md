# S059 — Context and Cache Usage Observability

**Status:** Active

## Purpose

Make provider prompt-cache usage visible without changing request construction,
context selection, compaction, or budget semantics. Journaled token usage remains
the canonical record used for replay and aggregate accounting.

## Scope

This slice touches:

- `simulacra-types` for the cross-boundary token-usage contract;
- `simulacra-provider` for OpenAI and Anthropic response parsing and metrics;
- `simulacra-runtime` for journal aggregation, replay, and run aggregation; and
- `simulacra-cli` only where exhaustively constructed or projected
  `TokenUsage` values must carry the new fields.

No provider request body, context-selection strategy, or compaction behavior
changes in S059.

## Token Usage Contract

`TokenUsage` has four counters:

- `input_tokens`: total logical provider-visible input tokens;
- `output_tokens`: generated output tokens;
- `cache_read_input_tokens`: input tokens served from a provider cache; and
- `cache_write_input_tokens`: input tokens written to a provider cache.

The cache counters are subsets of `input_tokens`. `TokenUsage::total()` remains
`input_tokens + output_tokens`; cache counters are not added again. Both cache
fields use Serde defaults so JSON written before S059 still deserializes as zero.

## Provider Accounting

OpenAI Chat Completions reports cached prompt tokens at
`usage.prompt_tokens_details.cached_tokens`. Its `prompt_tokens` already includes
cached tokens, so OpenAI maps:

- `input_tokens = prompt_tokens`;
- `cache_read_input_tokens = prompt_tokens_details.cached_tokens`; and
- `cache_write_input_tokens = 0`.

The same mapping applies to terminal usage objects in streaming responses.
Missing details map to zero.

Anthropic reports uncached input, cache reads, and cache creation separately.
For synchronous `usage` objects and streaming `message_start.usage` objects,
Anthropic maps:

- `cache_read_input_tokens = cache_read_input_tokens`;
- `cache_write_input_tokens = cache_creation_input_tokens`; and
- `input_tokens = input_tokens + cache_read_input_tokens +
  cache_creation_input_tokens`.

Missing Anthropic cache counters map to zero. The addition is saturating so
untrusted provider numbers cannot overflow.

## Persistence and Aggregation

Provider responses carry all four counters. `LlmResponse` journal entries
serialize them, replay restores them unchanged, and in-memory/SQLite journal
queries plus agent-run totals sum each counter independently. Budget charging
continues to use `TokenUsage::total()`, so cache counters are not double charged.

## Observability

Each successful OpenAI or Anthropic response, streaming or synchronous, records:

| OTel metric | Type | Value |
|---|---|---|
| `simulacra.context.cache.read_tokens` | `u64` histogram | `cache_read_input_tokens` |
| `simulacra.context.cache.write_tokens` | `u64` histogram | `cache_write_input_tokens` |
| `simulacra.context.cache.hit_ratio` | `f64` histogram | cache-read tokens divided by logical input tokens |

All three observations carry exact `gen_ai.provider.name` and
`gen_ai.request.model` labels. The hit ratio is `0.0` when logical input is zero.
Existing GenAI token-usage telemetry remains unchanged.

## Assertions

- [x] `TokenUsage` cache counters default to zero when absent from legacy JSON,
  round-trip when present, and do not increase `total()`.
- [x] OpenAI synchronous fixtures parse cached prompt tokens as a subset of
  logical input and leave cache writes at zero.
- [x] OpenAI streaming fixtures parse terminal cached prompt tokens as a subset
  of logical input and leave cache writes at zero.
- [x] Anthropic synchronous fixtures preserve cache-read/cache-creation
  counters and normalize logical input to their saturating sum with uncached
  input.
- [x] Anthropic streaming fixtures preserve cache-read/cache-creation counters
  from `message_start` and normalize logical input identically.
- [x] In-memory and SQLite journal round-trip/query paths preserve and
  independently aggregate both cache counters, while legacy `LlmResponse` JSON
  still deserializes.
- [x] Live and replayed agent runs independently aggregate cache counters and
  charge budgets only for logical input plus output.
- [x] Successful synchronous and streaming provider calls record all three
  cache metrics with exact provider/model labels, including a zero hit ratio
  when logical input is zero.

## Out of Scope

- Provider request cache controls or prefix reordering (S061).
- Context-window estimation or limits (S060).
- Observation masking, overflow recovery, or compaction (S062–S066).
