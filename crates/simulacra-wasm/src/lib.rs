//! WASM tool hosting for Simulacra.
//!
//! Loads `.wasm` tool modules via wasmtime (WASIp2 Component Model),
//! executes them in sandboxed WASI environments, and integrates with
//! Simulacra's tool system via the `Tool` trait.

mod error;
mod host;
mod tool;

pub use error::WasmError;
pub use host::WasmHost;
pub use tool::{WasiMount, WasiToolConfig, WasmTool};

wasmtime::component::bindgen!({
    world: "tool",
    path: "wit/simulacra-tool.wit",
});

/// Create `Tool` instances from a list of WASM tool configurations.
///
/// Each entry is `(name, module_path, fuel_limit, wasi_config)`. A
/// per-module `fuel_limit` of `0` means "unlimited for this tool"
/// (internally remapped to `u64::MAX`). Modules that fail to load or
/// discover are logged and skipped.
///
/// `agent_fuel_remaining` is an optional shared counter for the
/// **agent-level** fuel budget. Semantics (deliberately different from
/// the per-module knob):
///
/// - `None` — unlimited agent budget. Per-module limits still apply.
/// - `Some(Arc::new(AtomicU64::new(n)))` where `n > 0` — `n` fuel units
///   remain across all WASM tools spawned under this agent; each call
///   atomically reserves from this counter.
/// - `Some(Arc::new(AtomicU64::new(0)))` — **exhausted** (not
///   "unlimited"). Every call fails immediately with
///   `ToolError::ExecutionFailed`. Callers that want "unlimited" must
///   pass `None`, not a zero-valued counter.
pub fn create_wasm_tools(
    tools_config: &[(String, String, u64, WasiToolConfig)],
    agent_fuel_remaining: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
) -> Vec<Box<dyn simulacra_types::Tool>> {
    let mut host = match WasmHost::new() {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "failed to create WASM host; skipping all WASM tools");
            return Vec::new();
        }
    };

    let mut tools: Vec<Box<dyn simulacra_types::Tool>> = Vec::new();

    for (name, module_path, fuel, wasi_config) in tools_config {
        let path = std::path::Path::new(module_path);
        if let Err(e) = host.load_module(name, path) {
            tracing::warn!(
                module = %name,
                path = %module_path,
                error = %e,
                "failed to load WASM module; skipping"
            );
            continue;
        }

        let defs = match host.discover_tools(name) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    module = %name,
                    error = %e,
                    "failed to discover tools in WASM module; skipping"
                );
                continue;
            }
        };

        let engine = host.engine().clone();
        let component = match host.component(name) {
            Some(c) => c.clone(),
            None => continue,
        };

        for def in defs {
            tracing::info!(
                tool = %def.name,
                module = %name,
                fuel = fuel,
                "registered WASM tool"
            );
            let mut wasm_tool = WasmTool::new(
                engine.clone(),
                component.clone(),
                def,
                wasi_config.clone(),
                *fuel,
            );
            if let Some(ref arc) = agent_fuel_remaining {
                wasm_tool.set_agent_fuel(arc.clone());
            }
            tools.push(Box::new(wasm_tool));
        }
    }

    tools
}

#[cfg(test)]
mod tests {
    use wasmtime::component::Component;
    use wasmtime::{Config, Engine};

    #[test]
    fn echo_fixture_loads_as_component() {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("engine should create");
        let component = Component::from_file(&engine, "fixtures/echo-tool.wasm");
        assert!(
            component.is_ok(),
            "echo-tool.wasm should load: {:?}",
            component.err()
        );
    }
}
