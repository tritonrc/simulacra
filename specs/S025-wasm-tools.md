# S025 — WASM Tool Hosting

**Status:** Active
**Crate:** `simulacra-wasm` (new)

## Dependencies

- **S006** — Resource budgets (fuel integration with `ResourceBudget`)
- **S010** — Observability conventions
- **S012** — Built-in tools (`WasmTool` implements same `Tool` trait)

## Scope

Host WASM tool modules via wasmtime with WASIp2 sandbox, WIT-defined typed interface, fuel metering, and feature-gated CLI integration.

**In scope:**
- New `simulacra-wasm` crate (wasmtime, wasmtime-wasi, WASIp2 Component Model)
- WIT interface: `simulacra:tools/tool` world with `list-tools` and `call-tool` exports
- `WasmHost` — engine management, module loading/caching
- `WasmTool` — `Tool` trait implementation delegating to WASM module
- WASI sandbox: preopened filesystem dirs with permissions, env var allowlist, clocks, random
- No networking (disabled in S025)
- Fuel metering: per-module ceiling + agent-level `max_fuel` budget
- `[[wasm.tools]]` config section in `simulacra-config`
- CLI bootstrap wiring behind `features = ["wasm"]`
- Sample `echo-tool.wasm` fixture for testing

**Out of scope:**
- WASI networking (`wasi:sockets`, `wasi:http` — future spec)
- Simulacra host exports (`simulacra_journal_append`, `simulacra_capability_check` — S027)
- Governance hook pipeline (`before_tool_call`, etc. — S026)
- Tool distribution / registry / signing
- Pre-compiled `.cwasm` caching
- WASIp1 compatibility

## Context

Simulacra's architecture positions WASM as the universal extension layer — tools, governance hooks, and policy modules all hosted as `.wasm` files. S025 is the foundation: load WASM tool modules, execute them in a sandboxed WASI environment, and integrate them into the existing tool system.

The key properties: tools are sandboxed by construction (not by policy), fuel-metered (compute is bounded), and isolated per-invocation (no state leakage between calls). Enterprise value comes from the guarantee that a WASM tool *physically cannot* access resources outside its declared WASI grants.

S025 targets WASIp2 (the stable standard since January 2024) using the Component Model with WIT-defined interfaces. This gives compile-time type safety at the host-guest boundary and positions the codebase for future WASI capabilities (networking in a follow-up spec).

The `simulacra-wasm` crate is feature-gated — default Simulacra builds don't include wasmtime (~19MB). Enterprise users opt in with `--features wasm`.

## Design

### WIT interface

The contract between Simulacra and WASM tool modules:

```wit
// wit/simulacra-tool.wit

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

- `input-schema` is a JSON Schema string. The host doesn't interpret it — passes it through to the LLM.
- `arguments` is JSON-encoded. Tool authors parse it inside the module.
- `call-tool` returns JSON-encoded result on success, typed error on failure.
- A single module can export multiple tools via `list-tools`.
- No state between calls. The host creates a fresh `Store` per invocation.

### WasmHost

Owns the wasmtime `Engine`, loads and caches compiled `Component` instances:

```rust
pub struct WasmHost {
    engine: Engine,
    modules: HashMap<String, Component>,
}
```

- `Engine` created with `consume_fuel(true)` for metering.
- `Component::from_file()` compiles `.wasm` at startup (one-time cost per module).
- `WasmHost::new()` is fallible — returns `WasmError::ModuleLoadFailed` on engine init failure.

### WasmTool

Implements the `Tool` trait. One `WasmTool` per tool exported by a module:

```rust
pub struct WasmTool {
    engine: Engine,
    component: Component,
    tool_def: ToolDefinition,
    wasi_config: WasiToolConfig,
    fuel_limit: u64,
}
```

Each `call()` invocation:
1. Create fresh `Store` with WASI context (preopened dirs, env vars, clocks)
2. Set fuel: `store.set_fuel(min(module_limit, agent_remaining))`
3. Instantiate component via `bindgen!`-generated linker
4. Call `call-tool(name, arguments_json)`
5. Read fuel consumed: `initial_fuel - store.get_fuel()`
6. Report fuel to agent's `ResourceBudget`
7. Drop the `Store` — clean isolation

### WASI sandbox configuration

Per-module WASI grants configured in `simulacra.toml`:

```toml
[[wasm.tools]]
name = "file-ops"
module = "tools/file-ops.wasm"
fuel = 1_000_000

[wasm.tools.wasi]
fs = [
    { host = "/workspace", guest = "/data", perms = "rw" },
    { host = "/etc/templates", guest = "/templates", perms = "ro" },
]
env = ["GIT_TOKEN", "HOME"]
```

Mapping to `WasiCtxBuilder`:

| Config | WASI call |
|--------|-----------|
| `fs` + `perms = "ro"` | `preopened_dir(host, guest, DirPerms::READ, FilePerms::READ)` |
| `fs` + `perms = "rw"` | `preopened_dir(host, guest, DirPerms::all(), FilePerms::all())` |
| `env = ["X"]` | `env("X", std::env::var("X"))` — only listed vars |
| (always) | Clocks and random available |
| (always) | `allow_tcp(false)`, `allow_udp(false)` — no networking |
| (always) | No stdin inheritance |
| (always) | Stdout/stderr captured by host |

**What's NOT available:** networking, full environment, stdin, filesystem paths outside declared mounts.

### Fuel metering

Two layers:

**Per-module ceiling** (`fuel` in config):
- Set on `Store` before each `call-tool`
- Prevents a single call from running forever
- `0 = unlimited` (set to `u64::MAX`)
- `Trap::OutOfFuel` → `ToolError::ExecutionFailed("fuel exhausted")`

**Agent-level budget** (`max_fuel` on `ResourceBudget`):
- New field: `pub max_fuel: u64` (0 = unlimited)
- After each call, consumed fuel is subtracted from agent budget
- Budget exhausted → `ToolError::ExecutionFailed("fuel budget exhausted")` without instantiating
- Child agent fuel rolls up to parent (same as token budgets)

Per-call fuel is the minimum of both limits:
```
call_fuel = min(module_limit, agent_remaining)
            (with 0 treated as unlimited)
```

### Config types

```rust
// In simulacra-config
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmConfig {
    #[serde(default)]
    pub tools: Vec<WasmToolConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmToolConfig {
    pub name: String,
    pub module: String,
    #[serde(default)]
    pub fuel: u64,
    #[serde(default)]
    pub wasi: WasiToolConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WasiToolConfig {
    #[serde(default)]
    pub fs: Vec<WasiMount>,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasiMount {
    pub host: String,
    pub guest: String,
    #[serde(default = "default_perms")]
    pub perms: String,
}
```

`SimulacraConfig` gains: `pub wasm: Option<WasmConfig>`.

### Feature flag

`simulacra-wasm` is an optional dependency of `simulacra-cli`:

```toml
# simulacra-cli/Cargo.toml
[features]
default = []
wasm = ["dep:simulacra-wasm"]
```

CLI bootstrap wraps WASM tool loading in `#[cfg(feature = "wasm")]`. Default builds don't include wasmtime.

### Crate position in dependency graph

```
simulacra-types (leaf)
  ├→ simulacra-wasm (wasmtime, wasmtime-wasi, simulacra-types)
  ├→ simulacra-mcp (reqwest, simulacra-types)
  └→ ...
       └→ simulacra-cli (optional: simulacra-wasm, simulacra-mcp)
```

`simulacra-wasm` depends only on `simulacra-types` (for `ToolDefinition`, `ToolError`, `Tool` trait). It does not depend on `simulacra-tool`, `simulacra-sandbox`, or any other Simulacra crate.

## Behavior

### Module loading

1. `WasmHost::new()` creates a wasmtime `Engine` with fuel metering enabled.
2. `WasmHost::load_module(name, path)` compiles a `.wasm` file into a `Component` and caches it by name.
3. If the `.wasm` file doesn't exist or isn't a valid WASIp2 component, `WasmError::ModuleLoadFailed` is returned.
4. Module compilation happens once at startup. The compiled `Component` is reused across tool calls.

### Tool discovery

5. After loading a module, the host instantiates it once to call `list-tools()`.
6. Each `ToolDef` returned by `list-tools()` is bridged to a Simulacra `ToolDefinition`.
7. `input-schema` is parsed as JSON to verify it's valid JSON Schema. Invalid schemas are logged as warnings but not fatal.
8. One `WasmTool` instance is created per tool returned by `list-tools()`.

### Tool execution

9. Each `WasmTool::call()` creates a fresh wasmtime `Store` with a new WASI context.
10. WASI context is configured from `WasiToolConfig`: preopened dirs with declared permissions, allowlisted env vars, clocks, random. Networking disabled.
11. Fuel is set to `min(module_fuel_limit, agent_remaining_fuel)`, treating 0 as unlimited.
12. The component is instantiated via the bindgen-generated linker.
13. `call-tool(name, arguments_json)` is called on the instance.
14. On success, the JSON string result is returned as `serde_json::Value`.
15. On `Trap::OutOfFuel`, `ToolError::ExecutionFailed("fuel exhausted")` is returned.
16. On any other WASM trap, `ToolError::ExecutionFailed` is returned with the trap message.
17. On `tool-error::invalid-arguments`, `ToolError::ExecutionFailed` is returned with the message.
18. On `tool-error::execution-failed`, `ToolError::ExecutionFailed` is returned with the message.
19. After the call, fuel consumed is calculated. `WasmTool` exposes `last_fuel_consumed() -> u64` which the agent loop reads after each call to update the `ResourceBudget`. This avoids changing the `Tool` trait signature.
20. The `Store` is dropped. No state persists between calls.

### WASI sandbox enforcement

21. Filesystem access is restricted to preopened directories. Paths outside declared mounts cannot be accessed.
22. `perms = "ro"` grants read-only access. Write attempts fail with a WASI error.
23. `perms = "rw"` grants full read-write access within the mount.
24. Only env vars listed in `wasi.env` are visible. Other host env vars are not passed through.
25. TCP and UDP networking is disabled. `wasi:sockets` calls fail.
26. Clocks (`wall-clock`, `monotonic-clock`) and random (`random`) are available.

### Fuel metering

27. Per-module fuel ceiling prevents a single tool call from consuming unbounded compute.
28. Agent-level `max_fuel` budget bounds total WASM compute across all tool calls.
29. When agent fuel budget is exhausted, WASM tool calls fail without instantiation.
30. Fuel consumed by child agents rolls up to the parent budget (same as token budgets).
31. `0 = unlimited` for both per-module and agent-level fuel, consistent with all other budget fields.

### CLI integration

32. WASM tool loading is behind `#[cfg(feature = "wasm")]` in the CLI bootstrap.
33. Default builds (`cargo install simulacra`) do not include wasmtime.
34. `cargo install simulacra --features wasm` includes WASM tool support.
35. Module load failures during bootstrap are logged as warnings, not fatal.
36. Tools from successfully loaded modules are registered in `ToolRegistry` alongside builtins and MCP tools.

## Assertions

### Module loading

- [x] `WasmHost::new()` creates an engine with fuel metering enabled.
- [x] `load_module` compiles a valid `.wasm` WASIp2 component without error.
- [x] `load_module` returns `ModuleLoadFailed` for a nonexistent file.
- [x] `load_module` returns `ModuleLoadFailed` for an invalid (non-WASM) file.

### Tool discovery

- [x] `list-tools()` on the echo fixture returns a tool with name, description, and valid input schema.
- [x] Each discovered tool is bridged to a `ToolDefinition` with correct name, description, and input_schema.
- [x] A module exporting multiple tools produces multiple `WasmTool` instances.

### Tool execution

- [x] `call-tool("echo", json)` returns the correct result.
- [x] `call-tool("nonexistent", ...)` returns `ToolError`.
- [x] Each call uses a fresh `Store` — no state leaks between calls.
- [x] `WasmTool` implements the `Tool` trait and works through `ToolRegistry`.

### WASI sandbox

- [x] Tool can read files from a preopened `ro` directory.
- [x] Tool cannot write to a `ro` directory.
- [x] Tool can read and write files in a preopened `rw` directory.
- [x] Tool cannot access filesystem paths outside declared mounts.
- [x] Only allowlisted env vars are visible inside the module.
- [x] Non-allowlisted env vars return empty/None.
- [x] TCP/UDP network calls fail inside the module.

### Fuel metering

- [x] Tool call with sufficient fuel succeeds and reports fuel consumed > 0.
- [x] Tool call exceeding per-module fuel limit returns `ToolError` with "fuel exhausted".
- [x] Tool call when agent fuel budget is exhausted fails without instantiation.
- [x] Fuel consumed is subtracted from agent's `ResourceBudget`.
- [x] `fuel = 0` in config is treated as unlimited.
- [x] `max_fuel = 0` on `ResourceBudget` is treated as unlimited.

### CLI integration

- [x] With `features = ["wasm"]`, WASM tools from config are registered during bootstrap.
- [x] Without the feature flag, `[[wasm.tools]]` config is parsed but tools are not loaded.
- [x] Module load failure during bootstrap logs a warning and does not prevent startup.

## Observability (see S010)

- [x] `simulacra_wasm_tool_call` span wraps each tool invocation with `simulacra.wasm.module`, `simulacra.wasm.tool`, `simulacra.wasm.fuel_consumed`.
- [x] `simulacra_wasm_module_load` span wraps module compilation with `simulacra.wasm.module`, `simulacra.wasm.load_duration_ms`.
- [x] `simulacra.wasm.fuel_consumed` histogram records fuel per call with `module` and `tool` labels.
- [x] `simulacra.wasm.fuel_exhausted` counter incremented on fuel trap with `module` and `tool` labels.
- [x] `tracing::info!` on successful module load with tool count.
- [x] `tracing::warn!` on module load failure, fuel exhaustion, WASI violation.
- [x] `tracing::error!` on WASM trap (panic, unreachable, etc.).
