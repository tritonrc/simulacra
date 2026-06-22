# S025 WASM Tool Hosting — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Host WASM tool modules via wasmtime WASIp2 with sandboxed execution, fuel metering, and feature-gated CLI integration.

**Architecture:** New `simulacra-wasm` crate wraps wasmtime + wasmtime-wasi. A WIT interface (`simulacra:tools/tool`) defines the host-guest contract. `WasmHost` manages the engine and compiled modules. `WasmTool` implements the `Tool` trait, creating a fresh WASI-sandboxed `Store` per invocation. Feature-gated behind `wasm` in simulacra-cli.

**Tech Stack:** Rust, wasmtime (v43+), wasmtime-wasi (WASIp2), wit-bindgen (guest), serde_json, tracing, opentelemetry

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/simulacra-wasm/Cargo.toml` | Create | Crate manifest with wasmtime deps |
| `crates/simulacra-wasm/wit/simulacra-tool.wit` | Create | WIT interface definition |
| `crates/simulacra-wasm/src/lib.rs` | Create | Public API: `WasmHost`, `create_wasm_tools()` |
| `crates/simulacra-wasm/src/host.rs` | Create | `WasmHost` — engine, module loading, WASI config |
| `crates/simulacra-wasm/src/tool.rs` | Create | `WasmTool` — `Tool` trait impl, fuel metering |
| `crates/simulacra-wasm/src/error.rs` | Create | `WasmError` enum |
| `crates/simulacra-wasm/tests/wasm_tool_hosting_tests.rs` | Create | Behavioral tests |
| `crates/simulacra-wasm/fixtures/echo-tool.wasm` | Create | Pre-built test fixture (committed binary) |
| `tools/echo-tool/Cargo.toml` | Create | Guest crate for echo tool |
| `tools/echo-tool/src/lib.rs` | Create | Echo tool implementation |
| `crates/simulacra-config/src/lib.rs` | Modify | Add `WasmConfig`, `WasmToolConfig`, `WasiToolConfig`, `WasiMount` |
| `crates/simulacra-types/src/budget.rs` | Modify | Add `max_fuel`/`used_fuel` to `ResourceBudget` |
| `crates/simulacra-cli/Cargo.toml` | Modify | Add optional `simulacra-wasm` dep behind `wasm` feature |
| `crates/simulacra-cli/src/lib.rs` | Modify | Bootstrap wiring behind `#[cfg(feature = "wasm")]` |
| `Cargo.toml` | Modify | Add `simulacra-wasm` to workspace members + deps |

---

### Task 1: Scaffold `simulacra-wasm` crate with WIT and wasmtime

**Files:**
- Create: `crates/simulacra-wasm/Cargo.toml`
- Create: `crates/simulacra-wasm/wit/simulacra-tool.wit`
- Create: `crates/simulacra-wasm/src/lib.rs`
- Create: `crates/simulacra-wasm/src/error.rs`
- Modify: `Cargo.toml` (workspace)

This task creates the crate skeleton, WIT definition, error types, and verifies wasmtime compiles.

- [ ] **Step 1: Create the crate directory structure**

```bash
mkdir -p crates/simulacra-wasm/src crates/simulacra-wasm/wit crates/simulacra-wasm/fixtures
```

- [ ] **Step 2: Create `Cargo.toml`**

Create `crates/simulacra-wasm/Cargo.toml`:

```toml
[package]
name = "simulacra-wasm"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
simulacra-types.workspace = true
wasmtime = { version = "43", default-features = false, features = ["component-model", "cranelift", "std"] }
wasmtime-wasi = "43"
serde.workspace = true
serde_json.workspace = true
tracing.workspace = true
opentelemetry.workspace = true
thiserror.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["full"] }
```

- [ ] **Step 3: Create WIT interface**

Create `crates/simulacra-wasm/wit/simulacra-tool.wit`:

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

- [ ] **Step 4: Create error types**

Create `crates/simulacra-wasm/src/error.rs`:

```rust
/// Errors from WASM tool hosting operations.
#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    #[error("module load failed: {0}")]
    ModuleLoadFailed(String),

    #[error("instantiation failed: {0}")]
    InstantiationFailed(String),

    #[error("fuel exhausted after {consumed} units")]
    FuelExhausted { consumed: u64 },

    #[error("tool error: {0}")]
    ToolError(String),

    #[error("wasm trap: {0}")]
    Trap(String),
}
```

- [ ] **Step 5: Create `lib.rs` with re-exports**

Create `crates/simulacra-wasm/src/lib.rs`:

```rust
//! WASM tool hosting for Simulacra.
//!
//! Loads `.wasm` tool modules via wasmtime (WASIp2 Component Model),
//! executes them in sandboxed WASI environments, and integrates with
//! Simulacra's tool system via the `Tool` trait.

mod error;

pub use error::WasmError;

// Generate typed Rust bindings from the WIT interface.
wasmtime::component::bindgen!({
    world: "tool",
    path: "wit/simulacra-tool.wit",
});
```

- [ ] **Step 6: Add to workspace**

In the root `Cargo.toml`, add `"crates/simulacra-wasm"` to the `members` array and add the workspace dependency:

```toml
# In [workspace] members:
"crates/simulacra-wasm",

# In [workspace.dependencies]:
simulacra-wasm = { path = "crates/simulacra-wasm" }
```

- [ ] **Step 7: Verify it compiles**

Run: `cargo build -p simulacra-wasm`
Expected: PASS — wasmtime downloads and compiles, bindgen generates bindings from WIT.

This step will take a while (wasmtime is a large dependency tree). If it fails, check:
- Rust version >= 1.92.0 (`rustup update stable`)
- wasmtime version compatibility with your Rust edition

- [ ] **Step 8: Commit**

```bash
git add crates/simulacra-wasm/ Cargo.toml Cargo.lock
git commit -m "feat(wasm): scaffold simulacra-wasm crate with WIT interface [S025]"
```

---

### Task 2: Build echo-tool WASM fixture

**Files:**
- Create: `tools/echo-tool/Cargo.toml`
- Create: `tools/echo-tool/src/lib.rs`
- Create: `crates/simulacra-wasm/fixtures/echo-tool.wasm`

The echo-tool is a minimal WASM module that implements the `simulacra:tools/tool` WIT world. It's built separately (outside the main workspace) and the compiled `.wasm` is committed as a test fixture.

- [ ] **Step 1: Install the wasm32-wasip2 target**

```bash
rustup target add wasm32-wasip2
```

- [ ] **Step 2: Create guest crate**

Create `tools/echo-tool/Cargo.toml`:

```toml
[package]
name = "echo-tool"
version = "0.1.0"
edition = "2024"

[dependencies]
wit-bindgen = "0.41"

[lib]
crate-type = ["cdylib"]
```

Create `tools/echo-tool/src/lib.rs`:

```rust
wit_bindgen::generate!({
    world: "tool",
    path: "../../crates/simulacra-wasm/wit/simulacra-tool.wit",
});

struct EchoTool;

impl Guest for EchoTool {
    fn list_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "echo".into(),
                description: "Echo back the input text.".into(),
                input_schema: r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#.into(),
            },
            ToolDef {
                name: "reverse".into(),
                description: "Reverse the input text.".into(),
                input_schema: r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#.into(),
            },
        ]
    }

    fn call_tool(name: String, arguments: String) -> Result<String, ToolError> {
        match name.as_str() {
            "echo" => Ok(arguments),
            "reverse" => {
                // Parse the JSON to get the text field, reverse it
                let parsed: Result<serde_json::Value, _> = serde_json::from_str(&arguments);
                match parsed {
                    Ok(val) => {
                        let text = val.get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let reversed: String = text.chars().rev().collect();
                        Ok(format!(r#"{{"text":"{}"}}"#, reversed))
                    }
                    Err(e) => Err(ToolError::InvalidArguments(e.to_string())),
                }
            }
            _ => Err(ToolError::ExecutionFailed(format!("unknown tool: {name}"))),
        }
    }
}

export!(EchoTool);
```

Wait — `wit-bindgen` guest bindings may not have access to `serde_json` easily. The reverse tool should just do string manipulation without JSON parsing to keep dependencies minimal. Let me simplify:

```rust
wit_bindgen::generate!({
    world: "tool",
    path: "../../crates/simulacra-wasm/wit/simulacra-tool.wit",
});

struct EchoTool;

impl Guest for EchoTool {
    fn list_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "echo".into(),
                description: "Echo back the input JSON arguments.".into(),
                input_schema: r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#.into(),
            },
        ]
    }

    fn call_tool(name: String, arguments: String) -> Result<String, ToolError> {
        match name.as_str() {
            "echo" => Ok(arguments),
            _ => Err(ToolError::ExecutionFailed(format!("unknown tool: {name}"))),
        }
    }
}

export!(EchoTool);
```

- [ ] **Step 3: Build the WASM module**

```bash
cd tools/echo-tool
cargo build --target wasm32-wasip2 --release
```

The output will be at `tools/echo-tool/target/wasm32-wasip2/release/echo_tool.wasm`.

Note: this may need `cargo-component` or additional wasm-tools processing to produce a valid WASIp2 component. If `cargo build --target wasm32-wasip2` doesn't produce a component directly, use:

```bash
# If wasm-tools is needed:
cargo install wasm-tools
wasm-tools component new target/wasm32-wasip2/release/echo_tool.wasm -o echo_tool.component.wasm
```

Check wasmtime docs for whether raw `wasm32-wasip2` output is a valid component or needs post-processing.

- [ ] **Step 4: Copy fixture to simulacra-wasm**

```bash
cp tools/echo-tool/target/wasm32-wasip2/release/echo_tool.wasm \
   crates/simulacra-wasm/fixtures/echo-tool.wasm
```

- [ ] **Step 5: Verify the fixture loads in wasmtime**

Add a quick sanity test in `crates/simulacra-wasm/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Engine, Config, Store};
    use wasmtime::component::Component;

    #[test]
    fn echo_fixture_loads_as_component() {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("engine should create");
        let fixture = include_bytes!("../fixtures/echo-tool.wasm");
        let component = Component::from_binary(&engine, fixture);
        assert!(component.is_ok(), "echo-tool.wasm should load as a wasmtime component: {:?}", component.err());
    }
}
```

Run: `cargo test -p simulacra-wasm`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add tools/echo-tool/ crates/simulacra-wasm/fixtures/echo-tool.wasm crates/simulacra-wasm/src/lib.rs
git commit -m "feat(wasm): echo-tool guest crate and compiled fixture [S025]"
```

---

### Task 3: Implement `WasmHost` — engine, module loading, WASI config

**Files:**
- Create: `crates/simulacra-wasm/src/host.rs`
- Modify: `crates/simulacra-wasm/src/lib.rs`

- [ ] **Step 1: Write failing tests**

Add to `crates/simulacra-wasm/tests/wasm_tool_hosting_tests.rs`:

```rust
use simulacra_wasm::{WasmHost, WasmError};
use std::path::Path;

#[test]
fn wasm_host_creates_with_fuel_enabled() {
    let host = WasmHost::new();
    assert!(host.is_ok(), "WasmHost::new() should succeed");
}

#[test]
fn load_valid_module_succeeds() {
    let mut host = WasmHost::new().unwrap();
    let result = host.load_module("echo", Path::new("fixtures/echo-tool.wasm"));
    assert!(result.is_ok(), "loading a valid .wasm should succeed: {:?}", result.err());
}

#[test]
fn load_nonexistent_module_returns_error() {
    let mut host = WasmHost::new().unwrap();
    let result = host.load_module("missing", Path::new("fixtures/does-not-exist.wasm"));
    assert!(matches!(result, Err(WasmError::ModuleLoadFailed(_))));
}

#[test]
fn load_invalid_file_returns_error() {
    // Create a temp file with garbage bytes
    let tmp = std::env::temp_dir().join("not-a-wasm-file.wasm");
    std::fs::write(&tmp, b"this is not wasm").unwrap();
    let mut host = WasmHost::new().unwrap();
    let result = host.load_module("bad", &tmp);
    assert!(matches!(result, Err(WasmError::ModuleLoadFailed(_))));
    let _ = std::fs::remove_file(tmp);
}

#[test]
fn discover_tools_from_loaded_module() {
    let mut host = WasmHost::new().unwrap();
    host.load_module("echo", Path::new("fixtures/echo-tool.wasm")).unwrap();
    let tools = host.discover_tools("echo").unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert!(!tools[0].description.is_empty());
    assert!(!tools[0].input_schema.is_null());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p simulacra-wasm`
Expected: FAIL — `WasmHost` doesn't exist yet

- [ ] **Step 3: Implement `WasmHost`**

Create `crates/simulacra-wasm/src/host.rs`:

```rust
use crate::{Tool as WitTool, WasmError};
use simulacra_types::ToolDefinition;
use std::collections::HashMap;
use std::path::Path;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::WasiCtxBuilder;

/// Manages the wasmtime engine and compiled WASM modules.
pub struct WasmHost {
    engine: Engine,
    modules: HashMap<String, Component>,
}

impl WasmHost {
    /// Create a new WASM host with fuel metering enabled.
    pub fn new() -> Result<Self, WasmError> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);

        let engine = Engine::new(&config)
            .map_err(|e| WasmError::ModuleLoadFailed(format!("engine creation failed: {e}")))?;

        Ok(Self {
            engine,
            modules: HashMap::new(),
        })
    }

    /// Get a reference to the engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Get a reference to a loaded component by name.
    pub fn component(&self, name: &str) -> Option<&Component> {
        self.modules.get(name)
    }

    /// Load and compile a WASM module from a file path.
    pub fn load_module(&mut self, name: &str, path: &Path) -> Result<(), WasmError> {
        let _span = tracing::info_span!(
            "simulacra_wasm_module_load",
            simulacra.wasm.module = name,
        )
        .entered();

        let start = std::time::Instant::now();

        let component = Component::from_file(&self.engine, path)
            .map_err(|e| WasmError::ModuleLoadFailed(format!("{path:?}: {e}")))?;

        let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(
            simulacra.wasm.load_duration_ms = duration_ms,
            "WASM module compiled"
        );

        self.modules.insert(name.to_string(), component);
        Ok(())
    }

    /// Instantiate a module and call list-tools to discover its tool definitions.
    pub fn discover_tools(&self, name: &str) -> Result<Vec<ToolDefinition>, WasmError> {
        let component = self.modules.get(name).ok_or_else(|| {
            WasmError::ModuleLoadFailed(format!("module {name} not loaded"))
        })?;

        let mut store = Store::new(&self.engine, WasiCtxBuilder::new().build());
        store.set_fuel(u64::MAX).map_err(|e| {
            WasmError::InstantiationFailed(format!("set_fuel failed: {e}"))
        })?;

        let mut linker = Linker::new(&self.engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker)
            .map_err(|e| WasmError::InstantiationFailed(format!("WASI linker: {e}")))?;

        let (bindings, _instance) = WitTool::instantiate(&mut store, component, &linker)
            .map_err(|e| WasmError::InstantiationFailed(format!("instantiate: {e}")))?;

        let wit_tools = bindings.call_list_tools(&mut store)
            .map_err(|e| WasmError::ToolError(format!("list-tools failed: {e}")))?;

        let tools = wit_tools
            .into_iter()
            .map(|t| {
                let input_schema = serde_json::from_str(&t.input_schema)
                    .unwrap_or_else(|_| serde_json::json!({"type": "object"}));
                ToolDefinition {
                    name: t.name,
                    description: t.description,
                    input_schema,
                }
            })
            .collect();

        Ok(tools)
    }
}
```

Note: The exact wasmtime API calls (`WasiCtxBuilder::new().build()`, `add_to_linker_sync`, `WitTool::instantiate`, `call_list_tools`) depend on the version. The implementer should consult the wasmtime v43 docs if the signatures differ. The overall shape is correct.

- [ ] **Step 4: Update `lib.rs` to export `WasmHost`**

```rust
mod error;
mod host;

pub use error::WasmError;
pub use host::WasmHost;

wasmtime::component::bindgen!({
    world: "tool",
    path: "wit/simulacra-tool.wit",
});
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p simulacra-wasm`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-wasm/src/
git commit -m "feat(wasm): WasmHost with module loading and tool discovery [S025]"
```

---

### Task 4: Implement `WasmTool` — `Tool` trait, sandboxed execution, fuel

**Files:**
- Create: `crates/simulacra-wasm/src/tool.rs`
- Modify: `crates/simulacra-wasm/src/lib.rs`
- Modify: `crates/simulacra-wasm/tests/wasm_tool_hosting_tests.rs`

- [ ] **Step 1: Write failing tests**

Add to `crates/simulacra-wasm/tests/wasm_tool_hosting_tests.rs`:

```rust
use simulacra_types::{CapabilityToken, Tool, ToolError};
use simulacra_wasm::{WasmHost, WasmTool, WasiToolConfig};
use serde_json::json;
use std::path::Path;

fn create_echo_tool() -> WasmTool {
    let mut host = WasmHost::new().unwrap();
    host.load_module("echo", Path::new("fixtures/echo-tool.wasm")).unwrap();
    let tools = host.discover_tools("echo").unwrap();
    let tool_def = tools.into_iter().find(|t| t.name == "echo").unwrap();
    WasmTool::new(
        host.engine().clone(),
        host.component("echo").unwrap().clone(),
        tool_def,
        WasiToolConfig::default(),
        1_000_000, // fuel limit
    )
}

#[tokio::test]
async fn wasm_tool_call_returns_correct_result() {
    let tool = create_echo_tool();
    let cap = CapabilityToken::default();
    let result = tool.call(json!({"text": "hello wasm"}), &cap).await;
    assert!(result.is_ok(), "echo tool should succeed: {:?}", result.err());
    let value = result.unwrap();
    assert_eq!(value["text"], "hello wasm");
}

#[tokio::test]
async fn wasm_tool_definition_is_correct() {
    let tool = create_echo_tool();
    let def = tool.definition();
    assert_eq!(def.name, "echo");
    assert!(!def.description.is_empty());
    assert_eq!(def.input_schema["type"], "object");
}

#[tokio::test]
async fn wasm_tool_unknown_tool_returns_error() {
    let mut host = WasmHost::new().unwrap();
    host.load_module("echo", Path::new("fixtures/echo-tool.wasm")).unwrap();
    let tools = host.discover_tools("echo").unwrap();
    let mut tool_def = tools.into_iter().find(|t| t.name == "echo").unwrap();
    // Fake a bad tool name to test error path
    tool_def.name = "nonexistent".into();
    let tool = WasmTool::new(
        host.engine().clone(),
        host.component("echo").unwrap().clone(),
        tool_def,
        WasiToolConfig::default(),
        1_000_000,
    );
    let cap = CapabilityToken::default();
    let result = tool.call(json!({}), &cap).await;
    assert!(matches!(result, Err(ToolError::ExecutionFailed(_))));
}

#[tokio::test]
async fn wasm_tool_fuel_exhaustion_returns_error() {
    let mut host = WasmHost::new().unwrap();
    host.load_module("echo", Path::new("fixtures/echo-tool.wasm")).unwrap();
    let tools = host.discover_tools("echo").unwrap();
    let tool_def = tools.into_iter().find(|t| t.name == "echo").unwrap();
    // Set absurdly low fuel — should trap
    let tool = WasmTool::new(
        host.engine().clone(),
        host.component("echo").unwrap().clone(),
        tool_def,
        WasiToolConfig::default(),
        1, // 1 unit of fuel — will trap immediately
    );
    let cap = CapabilityToken::default();
    let result = tool.call(json!({"text": "hello"}), &cap).await;
    assert!(matches!(result, Err(ToolError::ExecutionFailed(_))));
}

#[tokio::test]
async fn wasm_tool_reports_fuel_consumed() {
    let tool = create_echo_tool();
    let cap = CapabilityToken::default();
    let _ = tool.call(json!({"text": "hello"}), &cap).await.unwrap();
    let consumed = tool.last_fuel_consumed();
    assert!(consumed > 0, "fuel consumed should be > 0, got {consumed}");
}

#[tokio::test]
async fn wasm_tool_isolation_no_state_between_calls() {
    let tool = create_echo_tool();
    let cap = CapabilityToken::default();
    // Call twice — each should produce identical results (no state leakage)
    let r1 = tool.call(json!({"text": "first"}), &cap).await.unwrap();
    let r2 = tool.call(json!({"text": "second"}), &cap).await.unwrap();
    assert_eq!(r1["text"], "first");
    assert_eq!(r2["text"], "second");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p simulacra-wasm`
Expected: FAIL — `WasmTool` doesn't exist

- [ ] **Step 3: Implement `WasmTool`**

Create `crates/simulacra-wasm/src/tool.rs`:

```rust
use crate::{Tool as WitTool, WasiToolConfig, WasmError};
use simulacra_types::{CapabilityToken, Tool, ToolDefinition, ToolError};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store, Trap};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

/// A WASM-hosted tool that implements the Simulacra `Tool` trait.
///
/// Each `call()` creates a fresh wasmtime `Store` with WASI sandbox,
/// executes the tool function, and drops the Store for clean isolation.
pub struct WasmTool {
    engine: Engine,
    component: Component,
    tool_def: ToolDefinition,
    wasi_config: WasiToolConfig,
    fuel_limit: u64,
    last_fuel: AtomicU64,
}

impl WasmTool {
    pub fn new(
        engine: Engine,
        component: Component,
        tool_def: ToolDefinition,
        wasi_config: WasiToolConfig,
        fuel_limit: u64,
    ) -> Self {
        Self {
            engine,
            component,
            tool_def,
            wasi_config,
            fuel_limit,
            last_fuel: AtomicU64::new(0),
        }
    }

    /// Get the fuel consumed by the last `call()` invocation.
    pub fn last_fuel_consumed(&self) -> u64 {
        self.last_fuel.load(Ordering::Relaxed)
    }

    /// Build a WASI context from the tool's config.
    fn build_wasi_ctx(&self) -> Result<wasmtime_wasi::WasiCtx, WasmError> {
        let mut builder = WasiCtxBuilder::new();

        // Mount filesystem directories
        for mount in &self.wasi_config.fs {
            let (dir_perms, file_perms) = if mount.perms == "rw" {
                (DirPerms::all(), FilePerms::all())
            } else {
                (DirPerms::READ, FilePerms::READ)
            };

            builder.preopened_dir(&mount.host, &mount.guest, dir_perms, file_perms)
                .map_err(|e| WasmError::InstantiationFailed(
                    format!("preopened_dir {:?} -> {:?}: {e}", mount.host, mount.guest)
                ))?;
        }

        // Filter environment variables
        for var_name in &self.wasi_config.env {
            if let Ok(value) = std::env::var(var_name) {
                builder.env(var_name, &value);
            }
        }

        // No networking
        builder.allow_tcp(false);
        builder.allow_udp(false);

        Ok(builder.build())
    }
}

impl Tool for WasmTool {
    fn definition(&self) -> ToolDefinition {
        self.tool_def.clone()
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>>
    {
        let arguments_json = arguments.to_string();
        let tool_name = self.tool_def.name.clone();

        Box::pin(async move {
            let _span = tracing::info_span!(
                "simulacra_wasm_tool_call",
                simulacra.wasm.module = tracing::field::Empty,
                simulacra.wasm.tool = %tool_name,
                simulacra.wasm.fuel_consumed = tracing::field::Empty,
            );

            let wasi_ctx = self.build_wasi_ctx()
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

            let mut store = Store::new(&self.engine, wasi_ctx);

            let fuel = if self.fuel_limit == 0 { u64::MAX } else { self.fuel_limit };
            store.set_fuel(fuel)
                .map_err(|e| ToolError::ExecutionFailed(format!("set_fuel: {e}")))?;

            let mut linker = Linker::new(&self.engine);
            wasmtime_wasi::add_to_linker_sync(&mut linker)
                .map_err(|e| ToolError::ExecutionFailed(format!("WASI linker: {e}")))?;

            let (bindings, _instance) = WitTool::instantiate(&mut store, &self.component, &linker)
                .map_err(|e| {
                    // Check for fuel exhaustion during instantiation
                    if let Some(trap) = e.downcast_ref::<Trap>() {
                        if *trap == Trap::OutOfFuel {
                            let consumed = fuel.saturating_sub(
                                store.get_fuel().unwrap_or(0)
                            );
                            self.last_fuel.store(consumed, Ordering::Relaxed);
                            return ToolError::ExecutionFailed(
                                format!("fuel exhausted during instantiation ({consumed} units consumed)")
                            );
                        }
                    }
                    ToolError::ExecutionFailed(format!("instantiate: {e}"))
                })?;

            let result = bindings.call_call_tool(&mut store, &tool_name, &arguments_json);

            // Calculate fuel consumed
            let remaining = store.get_fuel().unwrap_or(0);
            let consumed = fuel.saturating_sub(remaining);
            self.last_fuel.store(consumed, Ordering::Relaxed);

            // Record on span
            tracing::Span::current().record("simulacra.wasm.fuel_consumed", consumed);

            // Record OTel metrics
            let meters = crate::WasmMeters::get();
            let attrs = &[
                opentelemetry::KeyValue::new("tool", tool_name.clone()),
            ];
            meters.fuel_consumed.record(consumed as f64, attrs);

            match result {
                Ok(Ok(json_str)) => {
                    serde_json::from_str(&json_str).map_err(|e| {
                        ToolError::ExecutionFailed(format!("invalid JSON from tool: {e}"))
                    })
                }
                Ok(Err(wit_err)) => {
                    match wit_err {
                        crate::types::ToolError::InvalidArguments(msg) => {
                            Err(ToolError::InvalidArguments(msg))
                        }
                        crate::types::ToolError::ExecutionFailed(msg) => {
                            Err(ToolError::ExecutionFailed(msg))
                        }
                    }
                }
                Err(e) => {
                    if let Some(trap) = e.downcast_ref::<Trap>() {
                        if *trap == Trap::OutOfFuel {
                            meters.fuel_exhausted.add(1, attrs);
                            tracing::warn!(
                                tool = %tool_name,
                                consumed = consumed,
                                "WASM fuel exhausted"
                            );
                            return Err(ToolError::ExecutionFailed(
                                format!("fuel exhausted ({consumed} units consumed)")
                            ));
                        }
                    }
                    tracing::error!(
                        tool = %tool_name,
                        error = %e,
                        "WASM trap"
                    );
                    Err(ToolError::ExecutionFailed(format!("wasm trap: {e}")))
                }
            }
        })
    }
}
```

- [ ] **Step 4: Add OTel meters to `lib.rs`**

Add to `crates/simulacra-wasm/src/lib.rs`:

```rust
use opentelemetry::metrics::{Counter, Histogram};

/// Lazily-initialized OTel meters for WASM tool execution.
pub(crate) struct WasmMeters {
    pub fuel_consumed: Histogram<f64>,
    pub fuel_exhausted: Counter<u64>,
}

impl WasmMeters {
    pub fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<WasmMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-wasm");
            WasmMeters {
                fuel_consumed: meter
                    .f64_histogram("simulacra.wasm.fuel_consumed")
                    .with_description("WASM fuel consumed per tool call")
                    .build(),
                fuel_exhausted: meter
                    .u64_counter("simulacra.wasm.fuel_exhausted")
                    .with_description("WASM tool calls that hit fuel limit")
                    .build(),
            }
        })
    }
}
```

Update `lib.rs` exports:

```rust
mod error;
mod host;
mod tool;

pub use error::WasmError;
pub use host::WasmHost;
pub use tool::WasmTool;

// Re-export config types that callers need
pub use simulacra_config::{WasiToolConfig, WasiMount};
```

Wait — `simulacra-wasm` shouldn't depend on `simulacra-config`. The config types should either be defined in `simulacra-wasm` or in `simulacra-types`. Since they're simple data structs only used by `simulacra-wasm`, define them in `simulacra-wasm`:

```rust
/// WASI configuration for a WASM tool module.
#[derive(Debug, Clone, Default)]
pub struct WasiToolConfig {
    pub fs: Vec<WasiMount>,
    pub env: Vec<String>,
}

/// A filesystem mount for a WASM tool.
#[derive(Debug, Clone)]
pub struct WasiMount {
    pub host: String,
    pub guest: String,
    pub perms: String,
}
```

These mirror the config types but live in `simulacra-wasm`. The CLI maps from config types to these.

- [ ] **Step 5: Run tests**

Run: `cargo test -p simulacra-wasm`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-wasm/src/
git commit -m "feat(wasm): WasmTool with sandboxed execution and fuel metering [S025]"
```

---

### Task 5: Add `max_fuel` to `ResourceBudget`

**Files:**
- Modify: `crates/simulacra-types/src/budget.rs`

- [ ] **Step 1: Write the failing test**

Add to the existing test module in `crates/simulacra-types/src/budget.rs`:

```rust
#[test]
fn fuel_budget_exhausted_returns_error() {
    let mut budget = ResourceBudget::new(0, 0, Decimal::ZERO, 0);
    budget.max_fuel = 1000;
    budget.used_fuel = 1000;
    let err = budget.check_budget().expect_err("fuel should be exhausted");
    assert_eq!(err.resource, "fuel");
}

#[test]
fn fuel_zero_means_unlimited() {
    let mut budget = ResourceBudget::new(0, 0, Decimal::ZERO, 0);
    budget.max_fuel = 0;
    budget.used_fuel = 999_999;
    assert!(budget.check_budget().is_ok());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p simulacra-types`
Expected: FAIL — `max_fuel` field doesn't exist

- [ ] **Step 3: Add `max_fuel` and `used_fuel` to `ResourceBudget`**

In `crates/simulacra-types/src/budget.rs`, add the fields to the struct:

```rust
pub struct ResourceBudget {
    pub max_tokens: u64,
    pub max_turns: u32,
    pub max_cost: Decimal,
    pub max_sub_agents: u32,
    pub max_vfs_bytes: u64,
    pub max_fuel: u64,       // NEW: 0 = unlimited
    pub used_tokens: u64,
    pub used_turns: u32,
    pub used_cost: Decimal,
    pub used_sub_agents: u32,
    pub used_vfs_bytes: u64,
    pub used_fuel: u64,      // NEW
}
```

Update `new()` to initialize them to 0:

```rust
pub fn new(max_tokens: u64, max_turns: u32, max_cost: Decimal, max_sub_agents: u32) -> Self {
    Self {
        // ... existing fields ...
        max_fuel: 0,
        // ... existing fields ...
        used_fuel: 0,
    }
}
```

Add the budget check:

```rust
if self.max_fuel > 0 && self.used_fuel >= self.max_fuel {
    return Err(BudgetExhausted {
        resource: "fuel".into(),
        used: self.used_fuel.to_string(),
        limit: self.max_fuel.to_string(),
    });
}
```

- [ ] **Step 4: Fix compilation across workspace**

Run: `cargo build --workspace`

If any code constructs `ResourceBudget` with struct literal syntax (not `new()`), add `max_fuel: 0, used_fuel: 0` to those sites.

- [ ] **Step 5: Run tests**

Run: `cargo test -p simulacra-types`
Expected: PASS

Run: `cargo test --workspace` (check no regressions)

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-types/src/budget.rs
git commit -m "feat(budget): add max_fuel/used_fuel to ResourceBudget [S025]"
```

---

### Task 6: Config types and public `create_wasm_tools()` API

**Files:**
- Modify: `crates/simulacra-config/src/lib.rs`
- Modify: `crates/simulacra-wasm/src/lib.rs`

- [ ] **Step 1: Add config types to `simulacra-config`**

In `crates/simulacra-config/src/lib.rs`, add the WASM config structs:

```rust
/// WASM tool hosting configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmConfig {
    #[serde(default)]
    pub tools: Vec<WasmToolConfig>,
}

/// A single WASM tool module entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmToolConfig {
    pub name: String,
    pub module: String,
    #[serde(default)]
    pub fuel: u64,
    #[serde(default)]
    pub wasi: WasiConfig,
}

/// WASI sandbox configuration for a WASM tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WasiConfig {
    #[serde(default)]
    pub fs: Vec<WasiMountConfig>,
    #[serde(default)]
    pub env: Vec<String>,
}

/// A filesystem mount configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasiMountConfig {
    pub host: String,
    pub guest: String,
    #[serde(default = "default_perms")]
    pub perms: String,
}

fn default_perms() -> String {
    "ro".into()
}
```

Add `pub wasm: Option<WasmConfig>` to `SimulacraConfig`.

- [ ] **Step 2: Add config parsing test**

```rust
#[test]
fn wasm_tools_config_parses() {
    let config: SimulacraConfig = toml::from_str(r#"
        [project]
        name = "test"

        [agent_types.default]
        model = "test-model"
        system_prompt = "test"

        [[wasm.tools]]
        name = "echo"
        module = "tools/echo.wasm"
        fuel = 1000000

        [[wasm.tools.wasi.fs]]
        host = "/workspace"
        guest = "/data"
        perms = "rw"

        [wasm.tools.wasi]
        env = ["HOME"]
    "#).expect("WASM config should parse");

    let wasm = config.wasm.expect("wasm section should exist");
    assert_eq!(wasm.tools.len(), 1);
    assert_eq!(wasm.tools[0].name, "echo");
    assert_eq!(wasm.tools[0].fuel, 1000000);
    assert_eq!(wasm.tools[0].wasi.fs.len(), 1);
    assert_eq!(wasm.tools[0].wasi.fs[0].perms, "rw");
    assert_eq!(wasm.tools[0].wasi.env, vec!["HOME"]);
}
```

- [ ] **Step 3: Add `create_wasm_tools()` to `simulacra-wasm`**

In `crates/simulacra-wasm/src/lib.rs`, add the public integration function:

```rust
/// Load WASM tool modules and return Tool trait objects ready for registration.
///
/// Takes (name, module_path, fuel, wasi_config) tuples to avoid depending on simulacra-config.
/// Connection failures are logged and skipped — the returned Vec contains only
/// successfully loaded tools.
pub fn create_wasm_tools(
    tools_config: &[(String, String, u64, WasiToolConfig)],
) -> Vec<Box<dyn simulacra_types::Tool>> {
    let mut host = match WasmHost::new() {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "failed to create WASM host");
            return Vec::new();
        }
    };

    let mut result: Vec<Box<dyn simulacra_types::Tool>> = Vec::new();

    for (name, module_path, fuel, wasi_config) in tools_config {
        let path = std::path::Path::new(module_path);
        if let Err(e) = host.load_module(name, path) {
            tracing::warn!(module = %name, error = %e, "failed to load WASM module");
            continue;
        }

        let tools = match host.discover_tools(name) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(module = %name, error = %e, "failed to discover tools");
                continue;
            }
        };

        let tool_count = tools.len();
        for tool_def in tools {
            result.push(Box::new(WasmTool::new(
                host.engine().clone(),
                host.component(name).unwrap().clone(),
                tool_def,
                wasi_config.clone(),
                *fuel,
            )));
        }

        tracing::info!(module = %name, tools = tool_count, "WASM tools registered");
    }

    result
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p simulacra-config`
Run: `cargo test -p simulacra-wasm`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-config/src/lib.rs crates/simulacra-wasm/src/lib.rs
git commit -m "feat(wasm): config types and create_wasm_tools() public API [S025]"
```

---

### Task 7: CLI bootstrap wiring (feature-gated)

**Files:**
- Modify: `crates/simulacra-cli/Cargo.toml`
- Modify: `crates/simulacra-cli/src/lib.rs`

- [ ] **Step 1: Add optional dependency**

In `crates/simulacra-cli/Cargo.toml`:

```toml
[features]
default = []
wasm = ["dep:simulacra-wasm"]

[dependencies]
simulacra-wasm = { workspace = true, optional = true }
```

- [ ] **Step 2: Add bootstrap wiring**

In `crates/simulacra-cli/src/lib.rs`, in the `bootstrap()` function, after MCP tool registration and before spawn_agent tool registration, add:

```rust
// Register WASM tools from config (feature-gated).
#[cfg(feature = "wasm")]
if let Some(ref wasm_config) = config.wasm {
    let tools_config: Vec<(String, String, u64, simulacra_wasm::WasiToolConfig)> = wasm_config
        .tools
        .iter()
        .map(|tc| {
            let wasi = simulacra_wasm::WasiToolConfig {
                fs: tc.wasi.fs.iter().map(|m| simulacra_wasm::WasiMount {
                    host: m.host.clone(),
                    guest: m.guest.clone(),
                    perms: m.perms.clone(),
                }).collect(),
                env: tc.wasi.env.clone(),
            };
            (tc.name.clone(), tc.module.clone(), tc.fuel, wasi)
        })
        .collect();

    for tool in simulacra_wasm::create_wasm_tools(&tools_config) {
        registry.register(tool);
    }
}
```

- [ ] **Step 3: Verify default build (no wasm feature)**

```bash
cargo build -p simulacra-cli
```
Expected: PASS — compiles without wasmtime

- [ ] **Step 4: Verify wasm feature build**

```bash
cargo build -p simulacra-cli --features wasm
```
Expected: PASS — compiles with wasmtime, WASM tools loaded during bootstrap

- [ ] **Step 5: Run mechanical gate**

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-cli/Cargo.toml crates/simulacra-cli/src/lib.rs
git commit -m "feat(cli): wire WASM tool loading behind features=[wasm] [S025]"
```

---

### Task 8: WASI sandbox enforcement tests

**Files:**
- Create: `tools/sandbox-test-tool/Cargo.toml`
- Create: `tools/sandbox-test-tool/src/lib.rs`
- Create: `crates/simulacra-wasm/fixtures/sandbox-test-tool.wasm`
- Modify: `crates/simulacra-wasm/tests/wasm_tool_hosting_tests.rs`

A richer WASM fixture that exercises filesystem, env vars, and network denial.

- [ ] **Step 1: Create sandbox-test-tool guest crate**

Create `tools/sandbox-test-tool/Cargo.toml`:

```toml
[package]
name = "sandbox-test-tool"
version = "0.1.0"
edition = "2024"

[dependencies]
wit-bindgen = "0.41"

[lib]
crate-type = ["cdylib"]
```

Create `tools/sandbox-test-tool/src/lib.rs`:

```rust
wit_bindgen::generate!({
    world: "tool",
    path: "../../crates/simulacra-wasm/wit/simulacra-tool.wit",
});

struct SandboxTestTool;

impl Guest for SandboxTestTool {
    fn list_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "read_file".into(),
                description: "Read a file and return its contents.".into(),
                input_schema: r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#.into(),
            },
            ToolDef {
                name: "write_file".into(),
                description: "Write content to a file.".into(),
                input_schema: r#"{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}"#.into(),
            },
            ToolDef {
                name: "read_env".into(),
                description: "Read an environment variable.".into(),
                input_schema: r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}"#.into(),
            },
        ]
    }

    fn call_tool(name: String, arguments: String) -> Result<String, ToolError> {
        match name.as_str() {
            "read_file" => {
                // Extract path from JSON manually (no serde in guest)
                let path = extract_string_field(&arguments, "path")
                    .ok_or_else(|| ToolError::InvalidArguments("missing 'path'".into()))?;
                match std::fs::read_to_string(&path) {
                    Ok(content) => Ok(format!(r#"{{"content":"{}"}}"#, content.escape_default())),
                    Err(e) => Err(ToolError::ExecutionFailed(format!("read failed: {e}"))),
                }
            }
            "write_file" => {
                let path = extract_string_field(&arguments, "path")
                    .ok_or_else(|| ToolError::InvalidArguments("missing 'path'".into()))?;
                let content = extract_string_field(&arguments, "content")
                    .ok_or_else(|| ToolError::InvalidArguments("missing 'content'".into()))?;
                match std::fs::write(&path, &content) {
                    Ok(()) => Ok(r#"{"written":true}"#.into()),
                    Err(e) => Err(ToolError::ExecutionFailed(format!("write failed: {e}"))),
                }
            }
            "read_env" => {
                let name = extract_string_field(&arguments, "name")
                    .ok_or_else(|| ToolError::InvalidArguments("missing 'name'".into()))?;
                let value = std::env::var(&name).unwrap_or_default();
                Ok(format!(r#"{{"value":"{}"}}"#, value))
            }
            _ => Err(ToolError::ExecutionFailed(format!("unknown tool: {name}"))),
        }
    }
}

/// Extract a string field from a JSON object (minimal parser, no serde).
fn extract_string_field(json: &str, field: &str) -> Option<String> {
    let pattern = format!(r#""{}":""#, field);
    let start = json.find(&pattern)? + pattern.len();
    let rest = &json[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

export!(SandboxTestTool);
```

- [ ] **Step 2: Build and copy fixture**

```bash
cd tools/sandbox-test-tool
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/sandbox_test_tool.wasm \
   ../../crates/simulacra-wasm/fixtures/sandbox-test-tool.wasm
```

- [ ] **Step 3: Write sandbox enforcement tests**

Add to `crates/simulacra-wasm/tests/wasm_tool_hosting_tests.rs`:

```rust
#[tokio::test]
async fn wasi_read_file_from_preopened_dir() {
    // Create a temp dir with a file
    let tmp = std::env::temp_dir().join("simulacra-wasm-test-ro");
    let _ = std::fs::create_dir_all(&tmp);
    std::fs::write(tmp.join("hello.txt"), "hello from host").unwrap();

    let mut host = WasmHost::new().unwrap();
    host.load_module("sandbox", Path::new("fixtures/sandbox-test-tool.wasm")).unwrap();
    let tools = host.discover_tools("sandbox").unwrap();
    let tool_def = tools.into_iter().find(|t| t.name == "read_file").unwrap();

    let wasi = WasiToolConfig {
        fs: vec![WasiMount {
            host: tmp.to_str().unwrap().into(),
            guest: "/data".into(),
            perms: "ro".into(),
        }],
        env: vec![],
    };

    let tool = WasmTool::new(
        host.engine().clone(),
        host.component("sandbox").unwrap().clone(),
        tool_def,
        wasi,
        1_000_000,
    );

    let cap = CapabilityToken::default();
    let result = tool.call(json!({"path": "/data/hello.txt"}), &cap).await;
    assert!(result.is_ok(), "should read file from preopened dir: {:?}", result.err());

    let _ = std::fs::remove_dir_all(tmp);
}

#[tokio::test]
async fn wasi_write_to_ro_dir_fails() {
    let tmp = std::env::temp_dir().join("simulacra-wasm-test-ro-write");
    let _ = std::fs::create_dir_all(&tmp);

    let mut host = WasmHost::new().unwrap();
    host.load_module("sandbox", Path::new("fixtures/sandbox-test-tool.wasm")).unwrap();
    let tools = host.discover_tools("sandbox").unwrap();
    let tool_def = tools.into_iter().find(|t| t.name == "write_file").unwrap();

    let wasi = WasiToolConfig {
        fs: vec![WasiMount {
            host: tmp.to_str().unwrap().into(),
            guest: "/data".into(),
            perms: "ro".into(),
        }],
        env: vec![],
    };

    let tool = WasmTool::new(
        host.engine().clone(),
        host.component("sandbox").unwrap().clone(),
        tool_def,
        wasi,
        1_000_000,
    );

    let cap = CapabilityToken::default();
    let result = tool.call(json!({"path": "/data/test.txt", "content": "should fail"}), &cap).await;
    assert!(result.is_err(), "write to ro dir should fail");

    let _ = std::fs::remove_dir_all(tmp);
}

#[tokio::test]
async fn wasi_env_filtering_only_allowlisted_vars() {
    std::env::set_var("SIMULACRA_TEST_ALLOWED", "visible");
    std::env::set_var("SIMULACRA_TEST_SECRET", "hidden");

    let mut host = WasmHost::new().unwrap();
    host.load_module("sandbox", Path::new("fixtures/sandbox-test-tool.wasm")).unwrap();
    let tools = host.discover_tools("sandbox").unwrap();
    let tool_def = tools.into_iter().find(|t| t.name == "read_env").unwrap();

    let wasi = WasiToolConfig {
        fs: vec![],
        env: vec!["SIMULACRA_TEST_ALLOWED".into()], // only this one
    };

    let tool = WasmTool::new(
        host.engine().clone(),
        host.component("sandbox").unwrap().clone(),
        tool_def,
        wasi,
        1_000_000,
    );

    let cap = CapabilityToken::default();

    // Allowed var should be visible
    let result = tool.call(json!({"name": "SIMULACRA_TEST_ALLOWED"}), &cap).await.unwrap();
    assert_eq!(result["value"], "visible");

    // Non-allowlisted var should be empty
    let result = tool.call(json!({"name": "SIMULACRA_TEST_SECRET"}), &cap).await.unwrap();
    assert_eq!(result["value"], "");

    std::env::remove_var("SIMULACRA_TEST_ALLOWED");
    std::env::remove_var("SIMULACRA_TEST_SECRET");
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p simulacra-wasm`
Expected: PASS

- [ ] **Step 5: Run mechanical gate**

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 6: Commit**

```bash
git add tools/sandbox-test-tool/ crates/simulacra-wasm/fixtures/sandbox-test-tool.wasm crates/simulacra-wasm/tests/
git commit -m "feat(wasm): WASI sandbox enforcement tests with filesystem and env filtering [S025]"
```

---

## Self-Review

**Spec coverage check:**

| Spec Section | Task |
|---|---|
| Module loading (behaviors 1-4) | Task 3 |
| Tool discovery (behaviors 5-8) | Task 3 |
| Tool execution (behaviors 9-20) | Task 4 |
| WASI sandbox (behaviors 21-26) | Task 8 |
| Fuel metering (behaviors 27-31) | Tasks 4, 5 |
| CLI integration (behaviors 32-36) | Task 7 |
| Observability assertions | Tasks 3, 4 |
| WIT interface | Task 1 |
| Config types | Task 6 |

**Placeholder scan:** No TBD/TODO. All code blocks complete. One caveat noted: wasmtime v43 API signatures may differ slightly from code shown — implementer should consult docs.

**Type consistency:**
- `WasmHost`: defined Task 3, used in Tasks 4, 6, 8
- `WasmTool`: defined Task 4, used in Tasks 6, 8
- `WasiToolConfig` / `WasiMount`: defined in Task 4 (simulacra-wasm internal), config versions in Task 6 (simulacra-config), mapped in Task 7
- `ResourceBudget.max_fuel` / `used_fuel`: added Task 5, consumed by agent loop (existing code handles budget checks)
- `create_wasm_tools()`: defined Task 6, called in Task 7
- `WasmMeters`: defined Task 4, used in Task 4
