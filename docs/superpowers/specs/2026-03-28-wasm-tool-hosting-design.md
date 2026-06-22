# S025 — WASM Tool Hosting

**Status:** Active
**Crates involved:** `simulacra-wasm` (new), `simulacra-config`, `simulacra-cli`

## Dependencies

- **S006** — Resource budgets (fuel integration)
- **S010** — Observability conventions
- **S012** — Built-in tools (same `Tool` trait)

## Scope

Host WASM tool modules via wasmtime with WASIp2 sandbox, WIT-typed interface, fuel metering. Feature-gated behind `wasm`.

**In scope:** `simulacra-wasm` crate, WIT interface, `WasmHost`/`WasmTool`, WASI filesystem+env sandbox, dual fuel metering, `[[wasm.tools]]` config, CLI wiring, echo fixture.

**Out of scope:** WASI networking, Simulacra host exports (S027), governance hooks (S026), tool registry/distribution, `.cwasm` caching, WASIp1 compat.

Full spec: `specs/S025-wasm-tools.md`

## Design

### WIT Interface

```wit
package simulacra:tools@0.1.0;

interface types {
    record tool-def {
        name: string,
        description: string,
        input-schema: string,
    }
    variant tool-error {
        invalid-arguments(string),
        execution-failed(string),
    }
}

world tool {
    use types.{tool-def, tool-error};
    export list-tools: func() -> list<tool-def>;
    export call-tool: func(name: string, arguments: string) -> result<string, tool-error>;
}
```

- `input-schema` and `arguments` are JSON strings — keeps the interface generic
- Single module can export multiple tools
- No state between calls (fresh Store per invocation)

### Architecture

```
simulacra-types (leaf)
  ├→ simulacra-wasm (wasmtime, wasmtime-wasi, simulacra-types)
  ├→ simulacra-mcp (reqwest, simulacra-types)
  └→ ...
       └→ simulacra-cli (optional: simulacra-wasm via features=["wasm"])
```

### WasmHost + WasmTool

`WasmHost`: owns `Engine` (fuel enabled), compiles and caches `Component` instances. One per process.

`WasmTool`: implements `Tool` trait. Per call: fresh `Store` → WASI config → set fuel → instantiate → `call-tool` → read fuel → drop Store.

### WASI Sandbox

| Grant | WASI mapping |
|-------|-------------|
| `fs` + `"ro"` | `preopened_dir` with `DirPerms::READ` |
| `fs` + `"rw"` | `preopened_dir` with `DirPerms::all()` |
| `env = ["X"]` | Only listed vars passed |
| (always) | Clocks, random available |
| (always) | TCP/UDP disabled |

### Dual Fuel Metering

- Per-module ceiling: config `fuel` field, prevents runaway single calls
- Agent-level budget: `ResourceBudget.max_fuel`, bounds total WASM compute
- Per-call fuel = `min(module_limit, agent_remaining)`, 0 = unlimited
- Consumed fuel rolls up from child to parent agents

### Config

```toml
[[wasm.tools]]
name = "file-ops"
module = "tools/file-ops.wasm"
fuel = 1_000_000
[wasm.tools.wasi]
fs = [{ host = "/workspace", guest = "/data", perms = "rw" }]
env = ["GIT_TOKEN"]
```

### Feature Flag

`simulacra-cli` depends on `simulacra-wasm` optionally. `#[cfg(feature = "wasm")]` gates bootstrap loading. Default builds exclude wasmtime (~19MB).

### Testing

Committed `.wasm` fixtures built from Rust guest crates (outside main workspace):
- `echo-tool.wasm` — minimal tool for basic tests
- `sandbox-test-tool.wasm` — exercises filesystem, env, network denial, mutable state

### Observability

- `simulacra_wasm_tool_call` span per invocation (module, tool, fuel_consumed)
- `simulacra_wasm_module_load` span at startup (module, load_duration_ms)
- `simulacra.wasm.fuel_consumed` histogram, `simulacra.wasm.fuel_exhausted` counter
