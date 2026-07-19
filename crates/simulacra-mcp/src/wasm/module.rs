#[cfg(feature = "wasm")]
use std::sync::Arc;

#[cfg(feature = "wasm")]
use simulacra_types::{AgentId, JournalStorage};

use crate::error::McpError;
#[cfg(feature = "wasm")]
use crate::wasm::bindings::wit_server;
#[cfg(feature = "wasm")]
use crate::wasm::runtime::{
    build_default_wasm_http_client, build_wasm_mcp_linker, build_wasm_mcp_store,
};

/// A loaded WASM MCP server module.
///
/// Owns the compiled `wasmtime::component::Component` and the discovered
/// tool definitions. Per-call instantiation is cheap: a fresh `Store` and
/// `Linker` (with `wasi:cli` + `simulacra:mcp/http` host imports) per call.
///
/// Builder-style configuration on the module:
///   * `with_network_allowlist` — populates the `host:port` allowlist that
///     `simulacra:mcp/http.fetch` consults before any wire dispatch.
///   * `with_hooks` — registers a `simulacra_hooks::HookPipeline` that fetches
///     route through (`Operation::HttpRequest`, `Phase::Before`/`After`).
///   * `with_journal` — wires the journal that captures every fetch attempt.
///   * `with_fuel_limit` — overrides the per-call fuel ceiling.
///   * `with_http_client` — overrides the shared `reqwest::Client` so
///     enterprise proxy/CA/mTLS configuration can be threaded in from a
///     central location (e.g. a future `simulacra-http` async client).
#[cfg(feature = "wasm")]
pub struct WasmMcpModule {
    pub(crate) engine: wasmtime::Engine,
    pub(crate) component: wasmtime::component::Component,
    pub(crate) tools: Vec<simulacra_types::ToolDefinition>,
    pub(crate) allowlist: Vec<String>,
    pub(crate) hooks: Option<Arc<simulacra_hooks::HookPipeline>>,
    pub(crate) journal: Option<Arc<dyn JournalStorage>>,
    pub(crate) agent_id: AgentId,
    pub(crate) fuel_limit: u64,
    /// Shared `reqwest::Client` for `simulacra:mcp/http.fetch`. Built once
    /// at module load and cloned into each `WasmMcpServerState` so all
    /// outbound calls share connection-pool / proxy / TLS configuration.
    /// Cloning a `reqwest::Client` is cheap (it's `Arc<ClientInner>`
    /// internally).
    pub(crate) http_client: reqwest::Client,
}

#[cfg(feature = "wasm")]
impl WasmMcpModule {
    /// Replace the per-server `host:port` allowlist consulted by
    /// `simulacra:mcp/http.fetch`.
    pub fn with_network_allowlist(mut self, allowlist: Vec<String>) -> Self {
        self.allowlist = allowlist;
        self
    }

    /// Install a `simulacra_hooks::HookPipeline` that fetches from this module
    /// route through. The pipeline's `Operation::HttpRequest` chain is
    /// invoked at `Phase::Before` and `Phase::After` per fetch.
    pub fn with_hooks(mut self, hooks: Arc<simulacra_hooks::HookPipeline>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Install a journal so every fetch from this module is captured at
    /// the start of the request (Golden Rule).
    pub fn with_journal(mut self, journal: Arc<dyn JournalStorage>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Set the agent ID used when journaling fetches from this module.
    /// Empty AgentId is acceptable for shared-bootstrap deployments;
    /// per-agent attribution is a future spec.
    pub fn with_agent_id(mut self, agent_id: AgentId) -> Self {
        self.agent_id = agent_id;
        self
    }

    /// Override the per-call fuel ceiling. `0` = unlimited.
    pub fn with_fuel_limit(mut self, fuel_limit: u64) -> Self {
        self.fuel_limit = fuel_limit;
        self
    }

    /// Replace the shared `reqwest::Client` used by `simulacra:mcp/http.fetch`.
    ///
    /// Default is a HTTP/1.1-only client without connection pooling — the
    /// shape that the recording-fixture tests rely on. Production
    /// deployments that want HTTP/2, connection reuse, custom proxies, or
    /// custom CA bundles should pass a pre-configured client here.
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = client;
        self
    }
}

#[cfg(feature = "wasm")]
impl std::fmt::Debug for WasmMcpModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmMcpModule")
            .field(
                "tools",
                &self.tools.iter().map(|d| &d.name).collect::<Vec<_>>(),
            )
            .field("allowlist_entries", &self.allowlist.len())
            .field("hooks", &self.hooks.is_some())
            .field("journal", &self.journal.is_some())
            .field("agent_id", &self.agent_id)
            .field("fuel_limit", &self.fuel_limit)
            .finish()
    }
}

#[cfg(not(feature = "wasm"))]
#[derive(Debug, Clone, Default)]
pub struct WasmMcpModule;

/// Default per-call fuel ceiling for WASM MCP tools when no per-server
/// limit is configured. High enough that ordinary tools (echo, counter,
/// reverse) finish well below it; tight infinite loops (the `burn_fuel`
/// fixture) trap on `Trap::OutOfFuel` deterministically.
#[cfg(feature = "wasm")]
const DEFAULT_WASM_MCP_FUEL_PER_CALL: u64 = 10_000_000;

/// Discovery-time fuel budget — bounds the cost of `list-tools` so a
/// misbehaving module cannot hang `load_wasm_mcp_module`.
#[cfg(feature = "wasm")]
const DISCOVERY_FUEL_LIMIT: u64 = 1_000_000;

/// Compile a `.wasm` MCP server module and discover its tool exports.
///
/// Returns `McpError::ConnectionFailed` for compile failures, instantiation
/// failures, or `list-tools` traps — these all surface as connection-time
/// errors to the MCP layer.
#[cfg(feature = "wasm")]
pub fn load_wasm_mcp_module(path: &std::path::Path) -> Result<WasmMcpModule, McpError> {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    let engine = wasmtime::Engine::new(&config)
        .map_err(|e| McpError::ConnectionFailed(format!("wasmtime engine init failed: {e}")))?;

    let component = wasmtime::component::Component::from_file(&engine, path)
        .map_err(|e| McpError::ConnectionFailed(format!("wasm module compile failed: {e}")))?;

    // Discovery: instantiate once with a tight fuel budget, call list-tools.
    let runtime_handle = tokio::runtime::Handle::try_current().map_err(|_| {
        McpError::ConnectionFailed("load_wasm_mcp_module requires a tokio runtime".into())
    })?;
    // Discovery instantiation never calls `simulacra:mcp/http.fetch`, but
    // the state still needs *some* client. Use a default-constructed
    // one (we'll discard the store immediately after `list-tools`).
    let discovery_client = build_default_wasm_http_client();
    let mut store = build_wasm_mcp_store(
        &engine,
        DISCOVERY_FUEL_LIMIT,
        "<discovery>",
        Vec::new(),
        None,
        None,
        AgentId(String::new()),
        discovery_client.clone(),
        runtime_handle,
    )?;
    let linker = build_wasm_mcp_linker(&engine)?;
    let server = wit_server::Server::instantiate(&mut store, &component, &linker)
        .map_err(|e| McpError::ConnectionFailed(format!("wasm instantiation failed: {e}")))?;
    let wit_tools = server
        .call_list_tools(&mut store)
        .map_err(|e| McpError::ConnectionFailed(format!("wasm list-tools call failed: {e}")))?;

    let tools: Vec<simulacra_types::ToolDefinition> = wit_tools
        .into_iter()
        .map(|td| {
            let input_schema: serde_json::Value = serde_json::from_str(&td.input_schema)
                .unwrap_or_else(|_| serde_json::json!({"type": "object"}));
            simulacra_types::ToolDefinition {
                name: td.name,
                description: td.description,
                input_schema,
            }
        })
        .collect();

    Ok(WasmMcpModule {
        engine,
        component,
        tools,
        allowlist: Vec::new(),
        hooks: None,
        journal: None,
        agent_id: AgentId(String::new()),
        fuel_limit: DEFAULT_WASM_MCP_FUEL_PER_CALL,
        http_client: discovery_client,
    })
}

#[cfg(not(feature = "wasm"))]
pub fn load_wasm_mcp_module(_path: &std::path::Path) -> Result<WasmMcpModule, McpError> {
    Err(McpError::ConnectionFailed(
        "WASM MCP support is disabled (simulacra-mcp built without `wasm` feature)".to_string(),
    ))
}
