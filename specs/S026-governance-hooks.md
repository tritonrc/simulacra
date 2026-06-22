# S026 — Governance Hook Pipeline

**Status:** Active
**Crate:** `simulacra-hooks` (new)

## Dependencies

- **S003** — QuickJS runtime (JS hook execution)
- **S010** — Observability conventions
- **S025** — WASM tool hosting (future: WASM hook runtime, same interface)

## Scope

Rack-style middleware pipeline for governance hooks. Hooks wrap agent operations — inspect, modify, or deny. JS runtime for S026; WASM and Rust built-in runtimes are additive (same `invoke` interface).

**In scope:**
- New `simulacra-hooks` crate — pipeline framework, verdict model, hook chaining
- `HookModule` trait — runtime-agnostic interface
- `HookPipeline` — ordered middleware chains per operation type
- JS hook runtime via QuickJS with timeout enforcement
- 4 operation types: `tool_call`, `llm`, `spawn`, `http_request`
- Each operation has before + after phases (onion model)
- Verdicts: `Continue` (optionally modify), `Deny` (short-circuit), `Kill` (terminate agent)
- `[[hooks.*]]` config section
- Integration into `ToolRegistry`, agent loop, `SpawnAgentTool`, `UreqHttpClient`
- Sample JS hooks for testing
- Journal entries for denials and kills

**Out of scope:**
- WASM hook runtime (additive — same `HookModule` trait via `simulacra-wasm`)
- Rust built-in hooks (trivial — implement `HookModule` directly)
- Stateful hooks (sliding windows, counters, pattern detection over time)
- I/O hooks beyond HTTP (file, shell — future spec, same pipeline)
- Hook distribution / registry
- `on_budget_threshold` event hooks

## Context

Enterprise AI agents need governance that's enforced by the runtime, not trusted to the model. S026 implements a Rack-style middleware pipeline where hook modules wrap agent operations. Each hook can observe (log, meter), modify (redact PII, inject secrets), or decide (deny unauthorized actions, kill runaway agents).

Hooks are stateless — each invocation is independent. A single hook module handles one operation type. Multiple hooks on the same operation chain in config order (first-deny-wins). The pipeline drives the chain: before hooks run forward, the operation executes, after hooks run in reverse (onion model).

S026 ships with JS hooks via QuickJS. An admin writes a 20-line `.js` file, references it in `simulacra.toml`, done. WASM hooks (compiled, fuel-metered) use the same `invoke` interface and will be wired in a follow-up via the S025 infrastructure.

## Design

### Verdict model

```rust
pub enum Verdict {
    /// Proceed. Optionally replace the context JSON.
    Continue { modified_context: Option<String> },
    /// Block the operation. It does not execute.
    Deny { reason: String },
    /// Terminate the agent immediately.
    Kill { reason: String },
}

pub enum Phase {
    Before,
    After,
}

pub enum Operation {
    ToolCall,
    Llm,
    Spawn,
    HttpRequest,
}
```

### HookModule trait

```rust
/// Runtime-agnostic interface for hook execution.
/// Implemented by JsHookModule (S026), WasmHookModule (future), or Rust builtins.
pub trait HookModule: Send + Sync {
    fn name(&self) -> &str;
    fn invoke(
        &self,
        phase: Phase,
        operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError>;
}
```

### Pipeline execution

```rust
pub struct HookChain {
    hooks: Vec<Box<dyn HookModule>>,
}

pub struct HookPipeline {
    chains: HashMap<Operation, HookChain>,
}
```

`HookPipeline::wrap()` drives the onion:

1. Walk hooks forward (config order), calling `invoke(Before, op, ctx)` on each
2. If any returns `Deny` → stop, return error, operation does not execute
3. If any returns `Kill` → stop, return kill error, agent terminates
4. If `Continue` with modified context → pass modified context to next hook
5. Execute the operation with the (possibly modified) context
6. Walk hooks in reverse, calling `invoke(After, op, ctx_with_result)` on each
7. `Deny` in after-phase is ignored (operation already ran, logged as warning)
8. `Kill` in after-phase terminates the agent
9. `Continue` with modified context → pass modification to next hook
10. Return the (possibly modified) result

### Chaining semantics

- Hooks run in config order. First `Deny` or `Kill` stops the before-chain.
- After-hooks run in reverse config order (onion unwinding).
- Modifications chain — each hook sees the previous hook's output.
- Config order IS the priority. Security team puts their hook first.

### JS hook runtime

```rust
pub struct JsHookModule {
    name: String,
    script: String,
    timeout_ms: u64,
}
```

Each `invoke` call:
1. Create a fresh QuickJS runtime (no state between invocations)
2. Set interrupt callback with wall-clock deadline
3. Evaluate the module, call `invoke(phase, operation, context)`
4. Parse return value as verdict: `{ continue: null|string }`, `{ deny: string }`, `{ kill: string }`
5. If timeout fires → `HookError::Timeout` → treated as `Deny` (fail closed)
6. Drop the runtime

**JS hook contract:**

```javascript
// hooks/example.js
export function invoke(phase, operation, context) {
    const ctx = JSON.parse(context);
    // observe, modify, or decide
    return { continue: null };
}
```

### Timeout/fuel enforcement

| Runtime | Limit mechanism | Default | Exceed behavior |
|---------|----------------|---------|-----------------|
| JS | Wall-clock timeout (interrupt callback) | 100ms | Deny (fail closed) |
| WASM (future) | Fuel metering | 100,000 | Deny (fail closed) |
| Rust built-in (future) | None | N/A | Trusted |

Configurable per-hook in `simulacra.toml`.

### Context schemas

Each operation type passes a JSON context. Content varies by phase:

**`tool_call`:**
- Before: `{"tool": "shell_exec", "arguments": {"command": "git push"}}`
- After: `{"tool": "shell_exec", "arguments": {"command": "git push"}, "result": "..."}`

**`llm`:**
- Before: `{"model": "claude-sonnet-4-6", "message_count": 12}`
- After: `{"model": "claude-sonnet-4-6", "content": "...", "tool_calls": [...], "usage": {"input_tokens": 100, "output_tokens": 50}}`

**`spawn`:**
- Before: `{"agent_type": "researcher", "system_prompt": "...", "budget": {...}}`
- After: `{"agent_type": "researcher", "result": "...", "tokens_used": 1234}`

**`http_request`:**
- Before: `{"url": "https://api.github.com/...", "method": "GET", "headers": {...}, "body": null}`
- After: `{"url": "https://api.github.com/...", "method": "GET", "status": 200, "headers": {...}, "body": "..."}`

### Config

```toml
[[hooks.tool_call]]
name = "secret-injector"
runtime = "js"
module = "hooks/inject-secrets.js"
timeout_ms = 50

[[hooks.tool_call]]
name = "pii-scanner"
runtime = "js"
module = "hooks/scan-pii.js"
timeout_ms = 200

[[hooks.llm]]
name = "prompt-guard"
runtime = "js"
module = "hooks/prompt-guard.js"
timeout_ms = 100

[[hooks.http_request]]
name = "url-policy"
runtime = "js"
module = "hooks/url-policy.js"
timeout_ms = 50
```

Config types:

```rust
pub struct HooksConfig {
    pub tool_call: Vec<HookEntry>,
    pub llm: Vec<HookEntry>,
    pub spawn: Vec<HookEntry>,
    pub http_request: Vec<HookEntry>,
}

pub struct HookEntry {
    pub name: String,
    pub runtime: String,       // "js" for S026
    pub module: String,        // path to .js file
    pub timeout_ms: Option<u64>,  // default 100
}
```

### Integration points

**`tool_call`** — `ToolRegistry::call()` in `simulacra-tool`. Pipeline wraps the tool invocation.

**`llm`** — agent loop in `simulacra-runtime`. Pipeline wraps the `provider.chat()` call.

**`spawn`** — `SpawnAgentTool::call()` in `simulacra-runtime`. Pipeline wraps child creation + execution.

**`http_request`** — `UreqHttpClient::request()` in `simulacra-http`. Pipeline wraps the outbound HTTP call.

`HookPipeline` is `Arc<HookPipeline>`, created at bootstrap, passed to all integration points.

**`Kill` handling:** When a hook returns `Kill`, `HookError::Killed` propagates up. The agent loop catches it and terminates with `exit_reason: "PolicyKill"`. A `JournalEntryKind::HookKill` entry is recorded.

### Crate position

```
simulacra-types (leaf)
  ├→ simulacra-hooks (pipeline, verdict, HookModule trait — no runtime deps)
  ├→ simulacra-quickjs (existing — JS execution)
  └→ ...
       └→ simulacra-cli (creates JsHookModules, builds HookPipeline, passes to agent loop)
```

`simulacra-hooks` depends only on `simulacra-types` (for journal types). It does NOT depend on `simulacra-quickjs` — the JS runtime is wired by the CLI, not by the hooks crate. This keeps the framework runtime-agnostic.

## Behavior

### Pipeline execution

1. `HookPipeline::wrap()` runs the before-chain in config order.
2. If a before-hook returns `Deny`, the operation does not execute. `HookError::Denied` is returned.
3. If a before-hook returns `Kill`, the operation does not execute. `HookError::Killed` is returned.
4. If a before-hook returns `Continue` with modified context, the modified context is passed to the next hook and to the operation.
5. After all before-hooks pass, the operation executes.
6. After-hooks run in reverse config order.
7. If an after-hook returns `Deny`, it is logged as a warning and treated as `Continue` (operation already ran).
8. If an after-hook returns `Kill`, `HookError::Killed` is returned.
9. If an after-hook returns `Continue` with modified context, the modification is passed to the next after-hook and returned as the result.
10. The pipeline returns the final (possibly modified) result.

### JS hook execution

11. Each `invoke` call creates a fresh QuickJS runtime, separate from the agent's sandbox QuickJS. No state persists between invocations. Hook JS execution is isolated from agent JS execution.
12. The JS module is evaluated and its `invoke` function is called with `(phase, operation, context)`.
13. The return value is parsed as a verdict object with one key: `continue`, `deny`, or `kill`.
14. `{ continue: null }` → `Verdict::Continue { modified_context: None }`.
15. `{ continue: "json string" }` → `Verdict::Continue { modified_context: Some(json) }`.
16. `{ deny: "reason" }` → `Verdict::Deny { reason }`.
17. `{ kill: "reason" }` → `Verdict::Kill { reason }`.
18. Invalid return value → `HookError::ExecutionError`.

### Timeout enforcement

19. Before calling JS `invoke`, a wall-clock deadline is set.
20. QuickJS interrupt callback checks the deadline periodically.
21. If the deadline passes, JS execution is terminated.
22. Timeout is treated as `Deny` with reason `"hook timeout after {timeout_ms}ms"`. Fail closed.
23. Default timeout is 100ms if not configured.

### Integration

24. `ToolRegistry::call()` wraps tool invocations through the pipeline's `tool_call` chain.
25. Agent loop wraps `provider.chat()` through the pipeline's `llm` chain.
26. `SpawnAgentTool::call()` wraps spawn through the pipeline's `spawn` chain.
27. `UreqHttpClient::request()` wraps HTTP through the pipeline's `http_request` chain.
28. `HookPipeline` is `Arc<HookPipeline>`, created at bootstrap.
29. `Kill` verdict propagates as `HookError::Killed`. Agent loop terminates with `exit_reason: "PolicyKill"`.

### Journal

30. `Deny` verdicts produce a `JournalEntryKind::HookDenial` entry with hook name, operation, and reason.
31. `Kill` verdicts produce a `JournalEntryKind::HookKill` entry with hook name, operation, and reason.

### Config

32. `[[hooks.tool_call]]`, `[[hooks.llm]]`, `[[hooks.spawn]]`, `[[hooks.http_request]]` sections in `simulacra.toml`.
33. Each entry has `name`, `runtime`, `module`, and optional `timeout_ms`.
34. `runtime = "js"` is the only supported runtime in S026.
35. Hooks run in config order within each operation type.
36. Missing `[[hooks.*]]` sections means no hooks — operations run unmediated.

## Assertions

### Pipeline

- [x] Before-hooks run in config order.
- [x] After-hooks run in reverse config order (onion unwinding).
- [x] `Deny` in before-phase prevents the operation from executing.
- [x] `Kill` in before-phase prevents the operation and returns killed error.
- [x] `Deny` in after-phase is logged as warning and treated as `Continue`.
- [x] `Kill` in after-phase returns killed error.
- [x] `Continue` with modified context passes modification to next hook.
- [x] Modifications chain — each hook sees previous hook's output.
- [x] First-deny-wins — remaining before-hooks are not called after a deny.
- [x] Empty hook chain (no hooks configured) passes through unchanged.

### JS runtime

- [x] JS hook `invoke(phase, operation, context)` is called with correct arguments.
- [x] `{ continue: null }` returns `Verdict::Continue` with no modification.
- [x] `{ continue: "json" }` returns `Verdict::Continue` with modified context.
- [x] `{ deny: "reason" }` returns `Verdict::Deny` with reason.
- [x] `{ kill: "reason" }` returns `Verdict::Kill` with reason.
- [x] Invalid return value returns `HookError::ExecutionError`.
- [x] Each invocation uses a fresh runtime — no state between calls.
- [x] Timeout exceeded → treated as `Deny` (fail closed).
- [x] Default timeout is 100ms.

### Integration

- [x] `ToolRegistry::call()` runs `tool_call` hook chain around tool invocations.
- [x] Agent loop runs `llm` hook chain around `provider.chat()`.
- [x] `SpawnAgentTool::call()` runs `spawn` hook chain around child agent execution.
- [x] `UreqHttpClient::request()` runs `http_request` hook chain around outbound HTTP.
- [x] `Kill` verdict propagates to agent loop and terminates with `exit_reason: "PolicyKill"`.
- [x] Denied tool calls return `ToolError::ExecutionFailed` with the denial reason.

### Config

- [x] `[[hooks.tool_call]]` section is parsed into hook entries.
- [x] `[[hooks.llm]]` section is parsed into hook entries.
- [x] `[[hooks.spawn]]` section is parsed into hook entries.
- [x] `[[hooks.http_request]]` section is parsed into hook entries.
- [x] Missing hook sections mean no hooks (operations run unmediated).
- [x] Invalid runtime value returns a config error.

### Journal

- [x] `Deny` verdict produces a `HookDenial` journal entry with hook name, operation, reason.
- [x] `Kill` verdict produces a `HookKill` journal entry with hook name, operation, reason.

## Observability (see S010)

- [x] `simulacra_hook_invoke` span per hook invocation with `simulacra.hook.name`, `simulacra.hook.operation`, `simulacra.hook.phase`, `simulacra.hook.verdict`.
- [x] `simulacra.hooks.invocations` counter with `hook`, `operation`, `phase`, `verdict` labels.
- [x] `simulacra.hooks.denials` counter with `hook`, `operation` labels.
- [x] `simulacra.hooks.timeouts` counter with `hook`, `operation` labels.
- [x] `tracing::info!` on deny or kill with reason.
- [x] `tracing::warn!` on timeout (treated as deny).
- [x] `tracing::debug!` on each hook invocation.
