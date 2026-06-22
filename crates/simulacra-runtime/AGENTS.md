# AGENTS.md — simulacra-runtime

## Purpose

Top-level orchestration layer. Provides session management, journal storage,
agent supervision, the agent loop, and guardrail traits. Composes sandbox,
provider, context, and MCP into a running agent system.

## Key Types

- `Session` / `SessionStorage` / `InMemorySessionStorage` — conversation persistence.
- `InMemoryJournalStorage` — implements `simulacra_types::JournalStorage` in memory.
- `AgentSupervisor` — agent lifecycle management (stub).
- `AgentLoop` — core loop: message -> LLM -> tool calls -> journal (stub).
- `GuardrailDecision` / `InputGuardrail` / `OutputGuardrail` — pre/post message checks.
- `RuntimeError` — typed error enum for all runtime failures.

## Invariants

- Sessions are identified by string ID and bound to a single `AgentId`.
- Journal storage filters by `AgentId`; token usage queries sum only `LlmResponse` entries.
- All traits are object-safe (`Send + Sync + 'static`).

## Dependencies

`simulacra-types`, `simulacra-provider`, `simulacra-context`, `simulacra-sandbox`, `simulacra-mcp`,
`simulacra-tool`, `thiserror`, `tokio`, `tracing`, `serde`, `serde_json`.
