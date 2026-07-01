use std::sync::Arc;

use simulacra_types::{AgentId, JournalStorage};

use crate::error::McpError;
use crate::wasm::bindings::wit_server;
use crate::wasm::fetch::WASM_MCP_FETCH_DEFAULT_TIMEOUT;
use crate::wasm::fetch::{FetchError, FetchRequest, wasm_mcp_fetch_with_client_and_timeout};

/// Per-call store state for `simulacra:mcp/server` instances. Owns the WASI
/// context (required by `wasmtime_wasi::p2::add_to_linker_sync`) plus the
/// fetch context that the `simulacra:mcp/http.fetch` host import dispatches
/// through.
#[cfg(feature = "wasm")]
pub(crate) struct WasmMcpServerState {
    wasi_ctx: wasmtime_wasi::WasiCtx,
    table: wasmtime_wasi::ResourceTable,
    server_name: String,
    allowlist: Vec<String>,
    hooks: Option<Arc<simulacra_hooks::HookPipeline>>,
    journal: Option<Arc<dyn JournalStorage>>,
    agent_id: AgentId,
    /// Shared HTTP client borrowed from the owning [`WasmMcpModule`]. A
    /// clone of `reqwest::Client` is internally Arc-counted, so each
    /// per-call store holds a cheap handle into the same connection pool.
    http_client: reqwest::Client,
    /// Tokio runtime handle captured at the call site so the synchronous
    /// `simulacra:mcp/http.fetch` host import can drive the async
    /// [`wasm_mcp_fetch`] from inside `spawn_blocking`. Required because
    /// component-level host imports cannot be `async` directly — the
    /// bridge runs `runtime_handle.block_on(...)` to step into the
    /// surrounding tokio runtime instead of blocking the worker
    /// permanently.
    runtime_handle: tokio::runtime::Handle,
}

#[cfg(feature = "wasm")]
impl wasmtime_wasi::WasiView for WasmMcpServerState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.table,
        }
    }
}

/// Implementation of the `simulacra:mcp/http.fetch` host import. Dispatches to
/// the host-side `wasm_mcp_fetch` so allowlist + hooks + journal run as
/// they would for a Rust caller. Sync host fn → async fetch is bridged
/// via the runtime handle captured at module-call time.
#[cfg(feature = "wasm")]
impl wit_server::simulacra::mcp::http::Host for WasmMcpServerState {
    fn fetch(
        &mut self,
        req: wit_server::simulacra::mcp::http::Request,
    ) -> Result<
        wit_server::simulacra::mcp::http::Response,
        wit_server::simulacra::mcp::http::FetchError,
    > {
        let host_request = FetchRequest {
            method: req.method,
            url: req.url,
            headers: req.headers,
            body: req.body,
        };
        let result = self
            .runtime_handle
            .block_on(wasm_mcp_fetch_with_client_and_timeout(
                &self.server_name,
                host_request,
                &self.allowlist,
                self.hooks.as_deref(),
                self.journal.clone(),
                &self.agent_id,
                Some(&self.http_client),
                WASM_MCP_FETCH_DEFAULT_TIMEOUT,
            ));
        match result {
            Ok(resp) => Ok(wit_server::simulacra::mcp::http::Response {
                status: resp.status,
                headers: resp.headers,
                body: resp.body,
            }),
            Err(FetchError::CapabilityDenied(s)) => {
                Err(wit_server::simulacra::mcp::http::FetchError::CapabilityDenied(s))
            }
            Err(FetchError::HookDenied(s)) => {
                Err(wit_server::simulacra::mcp::http::FetchError::HookDenied(s))
            }
            Err(FetchError::Transport(s)) => {
                Err(wit_server::simulacra::mcp::http::FetchError::Transport(s))
            }
            Err(FetchError::Timeout) => Err(wit_server::simulacra::mcp::http::FetchError::Timeout),
        }
    }
}

/// Build a `Linker` that adds `wasi:cli` (so modules using e.g.
/// `wasi:cli/environment` link cleanly) plus the `simulacra:mcp/http` host
/// import.
#[cfg(feature = "wasm")]
pub(crate) fn build_wasm_mcp_linker(
    engine: &wasmtime::Engine,
) -> Result<wasmtime::component::Linker<WasmMcpServerState>, McpError> {
    let mut linker = wasmtime::component::Linker::<WasmMcpServerState>::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .map_err(|e| McpError::ConnectionFailed(format!("wasi linker setup failed: {e}")))?;
    wit_server::simulacra::mcp::http::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
        &mut linker,
        |state: &mut WasmMcpServerState| state,
    )
    .map_err(|e| {
        McpError::ConnectionFailed(format!("simulacra:mcp/http linker setup failed: {e}"))
    })?;
    Ok(linker)
}

/// Build a fresh `Store` seeded with a fuel budget plus a `WasmMcpServerState`
/// scoped to a single tool call. The server name + fetch context are
/// captured here so `simulacra:mcp/http.fetch` knows how to journal/route.
#[cfg(feature = "wasm")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_wasm_mcp_store(
    engine: &wasmtime::Engine,
    fuel: u64,
    server_name: &str,
    allowlist: Vec<String>,
    hooks: Option<Arc<simulacra_hooks::HookPipeline>>,
    journal: Option<Arc<dyn JournalStorage>>,
    agent_id: AgentId,
    http_client: reqwest::Client,
    runtime_handle: tokio::runtime::Handle,
) -> Result<wasmtime::Store<WasmMcpServerState>, McpError> {
    let wasi_ctx = wasmtime_wasi::WasiCtxBuilder::new().build();
    let state = WasmMcpServerState {
        wasi_ctx,
        table: wasmtime_wasi::ResourceTable::new(),
        server_name: server_name.to_string(),
        allowlist,
        hooks,
        journal,
        agent_id,
        http_client,
        runtime_handle,
    };
    let mut store = wasmtime::Store::new(engine, state);
    let fuel = if fuel == 0 { u64::MAX } else { fuel };
    store
        .set_fuel(fuel)
        .map_err(|e| McpError::ConnectionFailed(format!("set_fuel failed: {e}")))?;
    Ok(store)
}

/// Build the default `reqwest::Client` for `simulacra:mcp/http.fetch`. The
/// HTTP/1.1-only / pool-disabled / no-tcp_nodelay configuration mirrors
/// the recording-fixture-friendly settings the test suite relies on.
/// Production deployments inject their own client via
/// [`WasmMcpModule::with_http_client`].
#[cfg(feature = "wasm")]
pub(crate) fn build_default_wasm_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        // Coalesce small request payloads into a single TCP write so
        // recording fixtures that read once never miss the body.
        .tcp_nodelay(false)
        // Force HTTP/1.1 and disable connection reuse so every fetch is
        // a clean connect-write-read cycle. Recording fixtures read
        // once after accept and rely on the request bytes arriving
        // without HTTP/2 framing.
        .http1_only()
        .pool_max_idle_per_host(0)
        .build()
        .expect("default reqwest client should build")
}
