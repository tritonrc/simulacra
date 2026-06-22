# S043 — Provider Injection Seam + Stub-Provider E2E

**Status:** Active
**Crates:** `simulacra-server`

## Dependencies

- **S034** — SimulacraEngine spawn_task path (provider construction site)
- **S042** — agent catalog & GraphQL control plane (defers line 572 here)

## Scope

Close the deferred S042 §E2E line 572 assertion:
> Agent runs to completion against a recording HTTP fixture.

S042 v1 ships the GraphQL→catalog→SimulacraEngine seam end-to-end up to spawn (`graphql_e2e.rs::create_agent_via_graphql_then_spawn_task_resolves_the_catalog_row`). What's missing is exercising the agent loop *to terminal state* without requiring real `ANTHROPIC_API_KEY`/`OPENAI_API_KEY` env vars or an Ollama daemon.

This spec adds the smallest seam needed: an *optional* test-only `Provider` factory override on `SimulacraEngine` that lets a test substitute a scripted `Provider` impl for the production HTTP-backed providers.

**Out of scope (intentionally):**
- HTTP mocking (mockito/wiremock-rs). Line 572 says "recording HTTP fixture" but the assertion is about the engine+agent-loop seam, not the provider crate's HTTP code paths. A stub at the `Provider` trait boundary covers the spec; HTTP-level fixtures belong to a future provider-crate spec.
- A public `ProviderFactory` trait. The override is a test-only injection point, not a public API. Production code keeps the existing in-band construction.
- Provider crate changes (`simulacra-provider`).
- Multi-turn or tool-call replay scripting beyond what one e2e test needs. If the test needs more, the test grows the stub.

## Context

Today, `SimulacraEngine::spawn_task` constructs the LLM provider in-band:

```rust
// crates/simulacra-server/src/engine.rs (current)
let provider: Box<dyn simulacra_types::Provider> = match provider_kind {
    ProviderKind::Anthropic => Box::new(AnthropicProvider::new(&api_key, &model_clone)),
    ProviderKind::OpenAI | ProviderKind::Ollama => Box::new(OpenAiProvider::new(&api_key, &model_clone)),
};
```

`api_key` is read from `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`; if not set the spawn fails before the agent loop runs. Tests that try to assert on terminal state therefore can't run without external configuration.

## Design

### `ProviderFactory` type alias

A type alias (not a public trait — keeps the surface tight) over a closure:

```rust
// crates/simulacra-server/src/engine.rs
pub type ProviderFactory =
    Arc<dyn Fn(ProviderKind, &str /* model */) -> Result<Box<dyn simulacra_types::Provider>, EngineError>
        + Send + Sync>;
```

`SimulacraEngine` gains an optional field `provider_factory: Option<ProviderFactory>`. When `Some`, `spawn_task` calls it instead of the hardcoded match. When `None`, behavior is identical to today.

### Constructor surface

A new method `with_provider_factory(self, factory: ProviderFactory) -> Self` consumes-and-returns the engine for chaining. No change to existing `new` / `with_pool_config` / `with_components` / `with_memory` signatures (avoids cascading breakage across ~22 call sites).

### Test fixture

A `ScriptedProvider` in `crates/simulacra-server/tests/common/scripted_provider.rs` (or inline in `graphql_e2e.rs` if used by only one test):

```rust
/// A Provider that returns a fixed sequence of ProviderResponses, panics
/// if asked for more turns than the script provides.
struct ScriptedProvider {
    script: Mutex<VecDeque<ProviderResponse>>,
}
```

Single-turn happy path: scripted response is `ProviderResponse { stop_reason: StopReason::Stop, ... final assistant message ... }`. Engine maps `Stop` to `ExitReason::Complete` → `TaskState::Completed`.

### E2E test

In `crates/simulacra-server/tests/graphql_e2e.rs`:

```rust
#[tokio::test]
async fn agent_authored_via_graphql_runs_to_completion_under_scripted_provider() {
    // 1. catalog + schema (existing helpers)
    // 2. createAgent via GraphQL
    // 3. SimulacraEngine::with_provider_factory(scripted)
    // 4. spawn_task
    // 5. wait_for_terminal(handle.task_id, 5s)
    // 6. assert state == Completed
}
```

`wait_for_terminal` polls `TaskManager::get_task` (existing API).

## Behavior

### Provider override

- When `SimulacraEngine::with_provider_factory(f)` was called, `spawn_task` invokes `f(provider_kind, &model)` and uses the returned `Box<dyn Provider>` for the agent loop.
- When NOT called, `spawn_task` reads env vars and constructs the production provider exactly as today.
- The override is consulted *after* `build_provider(&model)` (env-var validation). Override means env-var validation is bypassed too — overriding implies the caller knows what they're doing.

### Scripted provider

- Returns the next response from the script on each `chat` call.
- If script is empty when `chat` is called, panics with a descriptive message (test bug, not runtime concern).
- Records each `chat` invocation's `messages` and `tools` for inspection.

## Assertions

### Provider injection

- [x] `SimulacraEngine::with_provider_factory(f)` returns an engine that uses `f` on `spawn_task`.
- [x] Engines constructed without `with_provider_factory` continue to require env vars (no behavior change for production paths).
- [x] `f` is invoked with the resolved `ProviderKind` and the agent's `model` string.
- [x] Test-only accessor `debug_provider_factory_is_set(&self) -> bool` returns true after `with_provider_factory`. *(Fail-fast: if a test forgets the override, this catches it.)*

### Scripted provider

- [x] `ScriptedProvider` returns scripted responses in order.
- [x] `ScriptedProvider` records each `chat` call's `messages` for assertion.
- [x] When script is exhausted, `chat` panics with a clear test-error message.

### E2E (closes S042 line 572)

- [x] An agent created via `createAgent` GraphQL mutation, spawned via `engine.spawn_task` with a `ScriptedProvider` override, reaches `TaskState::Completed` within 5s of spawn.
- [x] The `ScriptedProvider` is consulted at least once (proves the agent loop ran).
- [x] The system prompt the GraphQL mutation supplied appears in the messages the `ScriptedProvider` was called with (proves the catalog→engine→agent-loop chain carries the catalog data into the prompt).

## Observability

- [ ] `simulacra.engine.spawn_task` span (existing) captures `provider_overridden=true|false` attribute. *(deferred — minor signal; implementation site doesn't yet emit this attribute. Tracked alongside S042 §Observability follow-up.)*

## Open questions

1. Whether to expose `ScriptedProvider` as a public test-support helper in a future `simulacra-server-test-support` crate. v1 scope: keep it test-local. Promote when a second test wants it.
