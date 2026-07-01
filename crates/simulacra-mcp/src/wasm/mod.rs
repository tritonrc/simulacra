#[cfg(feature = "wasm")]
pub(crate) mod bindings;
pub(crate) mod fetch;
mod module;
#[cfg(feature = "wasm")]
mod runtime;

pub use fetch::{
    FetchError, FetchRequest, FetchResponse, check_network_allowlist, wasm_mcp_fetch,
    wasm_mcp_fetch_with_client_and_timeout, wasm_mcp_fetch_with_timeout,
};
pub use module::{WasmMcpModule, load_wasm_mcp_module};
#[cfg(feature = "wasm")]
pub(crate) use runtime::{build_wasm_mcp_linker, build_wasm_mcp_store};
