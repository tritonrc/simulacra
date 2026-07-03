# ARCHITECTURE.md — Simulacra

This document defines architectural positions and system invariants. It is the highest-authority reference after `AGENTS.md`. See `docs/simulacra-design.md` for full design rationale, diagrams, and prior art.

## System Identity

Simulacra is a single Rust binary that creates sandboxed agent environments with a virtual filesystem, shell emulator, and QuickJS runtime. No containers. No VMs. Agents configured via TOML.

## Crate Dependency Graph

```
simulacra-types (leaf: serde, schemars, rust_decimal only)
  ├→ simulacra-provider (reqwest, tokio, eventsource-stream)
  ├→ simulacra-tool (schemars, serde_json)
  ├→ simulacra-context
  ├→ simulacra-http (leaf: ureq, tracing)
  │    └→ simulacra-fetch (rquickjs, simulacra-http)
  └→ simulacra-vfs
       ├→ simulacra-shell
       └→ simulacra-quickjs (rquickjs, simulacra-fetch)
            └→ simulacra-sandbox (composes vfs + shell + quickjs + tool + http + fetch)
                 └→ simulacra-runtime (+ provider + context + mcp; tracing-opentelemetry, opentelemetry-otlp)
                      └→ simulacra-cli (ratatui, clap, anyhow)
```

**Invariant:** Dependencies flow strictly downward. The compiler enforces this.

## The Golden Rule

Everything the agent does that has side effects goes through the host. The QuickJS sandbox is a pure computation environment. Network, real FS, tools, sub-agent spawning — all mediated by Rust host functions that:

1. Check the `CapabilityToken`
2. Check the `ResourceBudget`
3. Write a `JournalEntry`
4. Emit an OTel span
5. Execute the operation
6. Return the result to the sandbox

## System Invariants

These are behavioral constraints that apply across all specs. They are non-negotiable.

### Error Handling

- Libraries: `thiserror`, typed error enums per crate.
- Binary (`simulacra-cli`): `anyhow`.
- Never `.unwrap()` in library code. Use `?` or explicit error handling.
- Every crate defines its own error enum. No cross-crate error re-exports.
- Error messages must be actionable: include what went wrong and what the caller can do about it.
- **Why:** Panics crash the whole process. With 50 agents running concurrently, one bad unwrap kills all of them. Typed errors let callers decide policy (retry, log, escalate).

### Journal Before Return

- Every side-effecting operation must write a `JournalEntry` **before** the result is returned to the agent.
- Side effects include: LLM calls, tool invocations, shell commands, JS code execution, HTTP requests, file writes, sub-agent spawn/complete.
- If you add a new side-effecting operation and forget to journal it, replay will diverge silently.
- **Why:** The journal is the basis for replay (Restate pattern) and fork-from-checkpoint (LangGraph pattern). A missing entry makes the journal incomplete, replay non-deterministic, and debugging impossible.

### No Child Processes for MCP

- Simulacra does not spawn child processes for MCP servers. No stdio transport. No `npx`. No `uvx`. No `Command::new()` for MCP.
- MCP servers are accessed via HTTP/SSE (now) or WASM in-process (future).
- **Why:** Single-binary philosophy. Spawning a child process implicitly requires that binary (and possibly Node.js or Python) on the host. This breaks the "cargo install simulacra and you're running" promise.
- **ADR:** See `docs/decisions/001-http-only-mcp.md`.

### Capabilities at the Call Site

- Check `CapabilityToken` in the proxy layer, not deep inside implementations.

### Budgets Before the Operation

- Check `ResourceBudget` before executing, not after. A limit of 0 means unlimited.

### OTel GenAI Semantic Conventions

- All LLM spans use `gen_ai.*` attributes per OTel GenAI Semantic Conventions v1.37+. No custom schemas.
- Simulacra-specific metrics use `simulacra.*` prefix only for non-GenAI concerns.

## Architectural Positions

### Async
- tokio only. One QuickJS runtime per `AgentCell`, never shared across tasks.
- Agent cells are `tokio::spawn` tasks. `mpsc` for messages. `Semaphore` for rate limiting.

### Traits
- All protocol traits are object-safe: `Send + Sync + 'static`, `Box<dyn Trait>`.
- Define trait → write tests against trait → implement.

### Types
- `rust_decimal::Decimal` for money. Never `f64`.
- `schemars::JsonSchema` on tool input structs.
- `serde::{Serialize, Deserialize}` on all cross-boundary types.

### Journal
- Append-only with periodic checkpoints (Restate pattern).
- Fork-from-checkpoint for debugging/retry (LangGraph pattern).
- Schema-versioned from day one.

### Supervision
- Actor-style on raw tokio. No framework dependency.
- Erlang-inspired: message priority (signals > supervision > commands > work).
- Policy-per-agent-type: restart strategies, resource budgets, capability tokens.
- Capability attenuation: children get a subset of parent's capabilities.

### QuickJS
- JS APIs implemented in Rust as host functions (AWS LLRT pattern), not JS polyfills.
- ESM only. `fs`, `process`, `console`, `fetch` modules.
- `rquickjs-serde` is allowed for typed Rust/QuickJS value conversion at host
  boundaries; avoid stringified JSON bridges for new runtime-facing APIs.

### Dependencies
- Justified external deps: `rmcp`, `rquickjs`, `rquickjs-serde`, `reqwest`, `url`, `tokio`, `serde`, `schemars`, `rust_decimal`, `ratatui`, `clap`, `toml`, `thiserror`, `anyhow`, `tracing`, `tracing-opentelemetry`, `opentelemetry-otlp`.
- We build our own: Provider impls, Tool registry, agent loop, context management, sessions, guardrails.
- Before adding any dep: is it maintained? >1000 downloads? Could we write it in <200 lines?
