//! WasmHost: engine management, module loading, and tool discovery.

use std::collections::HashMap;
use std::path::Path;

use wasmtime::component::Component;
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::Tool as WasmToolBindings;
use crate::error::WasmError;

/// Fuel budget for `list-tools` discovery. Enough for typical
/// enumeration but bounded so a malicious or buggy module cannot hang
/// the bootstrap path.
const DISCOVER_FUEL_LIMIT: u64 = 1_000_000;

/// Host state for WASI component instantiation.
struct WasmState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for WasmState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// Manages a wasmtime engine and cached compiled components.
///
/// Provides module loading and tool discovery for WASM tool modules
/// that implement the `simulacra:tools/tool` WIT world.
pub struct WasmHost {
    engine: Engine,
    modules: HashMap<String, Component>,
}

impl WasmHost {
    /// Create a new WasmHost with component model and fuel metering enabled.
    pub fn new() -> Result<Self, WasmError> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);
        let engine =
            Engine::new(&config).map_err(|e| WasmError::ModuleLoadFailed(e.to_string()))?;
        Ok(Self {
            engine,
            modules: HashMap::new(),
        })
    }

    /// Reference to the underlying wasmtime engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Look up a cached component by name.
    pub fn component(&self, name: &str) -> Option<&Component> {
        self.modules.get(name)
    }

    /// Compile a `.wasm` file into a Component and cache it under `name`.
    pub fn load_module(&mut self, name: &str, path: &Path) -> Result<(), WasmError> {
        let _span = tracing::info_span!(
            "simulacra_wasm_module_load",
            simulacra.wasm.module = %name,
            simulacra.wasm.path = %path.display(),
        )
        .entered();

        let start = std::time::Instant::now();
        let component = Component::from_file(&self.engine, path).map_err(|e| {
            tracing::error!(
                module = %name,
                path = %path.display(),
                error = %e,
                "WASM module compilation failed"
            );
            WasmError::ModuleLoadFailed(e.to_string())
        })?;
        let load_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            module = %name,
            simulacra.wasm.load_duration_ms = load_ms,
            "WASM module compiled successfully"
        );

        self.modules.insert(name.to_string(), component);
        Ok(())
    }

    /// Instantiate a cached component and call `list-tools` to discover
    /// the tool definitions it exports.
    ///
    /// Returns `ToolDefinition` values with `input_schema` parsed as JSON.
    pub fn discover_tools(
        &self,
        name: &str,
    ) -> Result<Vec<simulacra_types::ToolDefinition>, WasmError> {
        let component = self
            .modules
            .get(name)
            .ok_or_else(|| WasmError::ModuleLoadFailed(format!("module '{}' not loaded", name)))?;

        let wasi_ctx = WasiCtxBuilder::new().build();
        let state = WasmState {
            ctx: wasi_ctx,
            table: ResourceTable::new(),
        };
        let mut store = Store::new(&self.engine, state);

        // Discovery runs with a bounded fuel budget. A misbehaving module
        // that allocates inside its `list-tools` implementation should not
        // be able to hang the bootstrap path.
        store
            .set_fuel(DISCOVER_FUEL_LIMIT)
            .map_err(|e| WasmError::InstantiationFailed(e.to_string()))?;

        let mut linker = wasmtime::component::Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| WasmError::InstantiationFailed(e.to_string()))?;

        let bindings = WasmToolBindings::instantiate(&mut store, component, &linker)
            .map_err(|e| WasmError::InstantiationFailed(e.to_string()))?;

        let wit_tools = bindings.call_list_tools(&mut store).map_err(|e| {
            // Distinguish a fuel-exhausted discovery from other traps so
            // the operator sees an actionable error.
            if e.downcast_ref::<wasmtime::Trap>()
                .is_some_and(|t| *t == wasmtime::Trap::OutOfFuel)
            {
                tracing::error!(
                    module = %name,
                    fuel_limit = DISCOVER_FUEL_LIMIT,
                    "WASM tool discovery exhausted fuel budget"
                );
                WasmError::InstantiationFailed(format!(
                    "module '{}' exhausted {} fuel during list-tools",
                    name, DISCOVER_FUEL_LIMIT
                ))
            } else {
                WasmError::InstantiationFailed(e.to_string())
            }
        })?;

        Ok(wit_tools
            .into_iter()
            .map(|td| {
                let input_schema: serde_json::Value = match serde_json::from_str(&td.input_schema) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            tool = %td.name,
                            error = %e,
                            raw_schema = %td.input_schema,
                            "invalid input_schema JSON; using fallback {{\"type\": \"object\"}}"
                        );
                        serde_json::json!({"type": "object"})
                    }
                };
                simulacra_types::ToolDefinition {
                    name: td.name,
                    description: td.description,
                    input_schema,
                }
            })
            .collect())
    }
}
