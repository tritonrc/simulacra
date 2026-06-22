# S026 — Governance Hook Pipeline

**Status:** Active
**Crates involved:** `simulacra-hooks` (new), `simulacra-tool`, `simulacra-runtime`, `simulacra-http`, `simulacra-cli`

## Dependencies

- **S003** — QuickJS runtime (JS hook execution)
- **S010** — Observability conventions

## Scope

Rack-style middleware pipeline for governance hooks with JS runtime. Hooks wrap agent operations — observe, modify, or decide. Four operation types (tool_call, llm, spawn, http_request), each with before/after onion model.

Full spec: `specs/S026-governance-hooks.md`

## Design

### Rack Middleware Model

Hooks wrap operations like Rack middlewares. The pipeline drives the chain — not continuation-passing:

```
before_1 → before_2 → [execute operation] → after_2 → after_1
```

Three modes of hook behavior:
- **Observe** — inspect, log, meter. Returns `Continue(null)`.
- **Modify** — transform input or output. Returns `Continue(modified_json)`.
- **Decide** — allow or deny. Returns `Deny(reason)` or `Kill(reason)`.

### Verdict Model

`Continue(Option<String>)` — proceed, optionally replace context.
`Deny(String)` — block operation (before-phase only).
`Kill(String)` — terminate the agent.

First-deny-wins. Config order is priority. After-hooks cannot deny (operation already ran) but can kill or modify.

### JS Hook Contract

```javascript
export function invoke(phase, operation, context) {
    const ctx = JSON.parse(context);
    return { continue: null };  // or { deny: "reason" } or { kill: "reason" }
}
```

Fresh QuickJS runtime per invocation. Wall-clock timeout (default 100ms). Timeout = deny (fail closed).

### Four Operation Types

| Operation | Before context | After context |
|-----------|---------------|---------------|
| `tool_call` | `{tool, arguments}` | `+ result` |
| `llm` | `{model, message_count}` | `+ content, tool_calls, usage` |
| `spawn` | `{agent_type, system_prompt, budget}` | `+ result, tokens_used` |
| `http_request` | `{url, method, headers, body}` | `+ status, response headers, response body` |

### Integration Points

- `ToolRegistry::call()` — wraps tool invocations
- Agent loop — wraps `provider.chat()`
- `SpawnAgentTool::call()` — wraps child agent execution
- `UreqHttpClient::request()` — wraps outbound HTTP

`HookPipeline` is `Arc<HookPipeline>`, created at bootstrap, passed to all call sites.

### Config

```toml
[[hooks.tool_call]]
name = "pii-scanner"
runtime = "js"
module = "hooks/scan-pii.js"
timeout_ms = 200
```

### Crate Position

`simulacra-hooks` depends only on `simulacra-types`. Runtime-agnostic — JS/WASM/Rust builtins are wired by the CLI, not by the hooks crate.
