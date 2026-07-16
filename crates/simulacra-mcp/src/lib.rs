//! Simulacra MCP (Model Context Protocol) crate.
//!
//! Manages connections to external MCP servers over HTTP/SSE and WASM
//! transports and exposes their tools as `ToolDefinition` values.
//!
//! Per R002: this crate MUST NOT use `std::process::Command` or
//! `tokio::process` -- all communication happens over network transports
//! or in-process WASM components.

mod bootstrap;
mod catalog;
mod domain;
mod error;
mod manager;
mod observability;
mod tool;
mod transport;
mod wasm;

pub use bootstrap::{WasmMcpServerDescriptor, create_mcp_tools, create_mcp_tools_with_wasm};
pub use catalog::{
    McpCallTool, McpCatalog, McpSearchTool, McpServerDescriptor, McpServerKind,
    WasmMcpServerDescriptor as DeferredWasmMcpServerDescriptor,
};
pub use domain::transport_config::parse_wasm_transport;
pub use error::McpError;
pub use manager::McpManager;
pub use simulacra_types::ToolDefinition;
pub use tool::McpTool;
pub use transport::headers::redact_headers_for_log;
pub use wasm::{
    FetchError, FetchRequest, FetchResponse, WasmMcpModule, check_network_allowlist,
    load_wasm_mcp_module, wasm_mcp_fetch, wasm_mcp_fetch_with_client_and_timeout,
    wasm_mcp_fetch_with_timeout,
};
