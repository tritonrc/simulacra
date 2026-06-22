//! Authoring SDK for WASM MCP servers (S041).
//!
//! Authors decorate Rust functions with `#[mcp_tool]` and the macro generates
//! the registration glue. At runtime (or in unit tests on the host), the SDK
//! exposes:
//!
//! * [`list_tools`] — enumerates every `#[mcp_tool]`-annotated function in the
//!   compiled binary, returning a [`ToolDef`] per tool with the JSON-Schema
//!   derived `input_schema`.
//! * [`call_tool`] — dispatches a JSON-encoded payload to the named tool's
//!   function, returning the JSON-serialized result.
//! * [`fetch`] helpers — wrap the imported `simulacra:mcp/http.fetch` interface for
//!   modules running in `wasm32-wasip2`. On host targets (the Rust test build),
//!   the helpers route through a process-global recorder so tests can assert
//!   that the SDK's outbound HTTP path is exercised.
//!
//! The fixtures module ships two sample tools (`echo`, `fetch_helper`) so the
//! macro tests have something to enumerate without depending on
//! `wasm32-wasip2`. Authors are expected to define their own tools in their
//! own crate; the fixtures exist purely to keep the SDK self-testable.

// Allow the `#[mcp_tool]` proc-macro expansion (which references
// `::simulacra_mcp_server_sdk::...`) to compile inside this crate too. Without
// this alias, in-crate uses of the macro (the `fixtures` module + dogfood
// tests) cannot resolve the absolute path.
extern crate self as simulacra_mcp_server_sdk;

pub use simulacra_mcp_server_sdk_macro::mcp_tool;

pub mod fetch;
pub mod fixtures;

// ---- Public types --------------------------------------------------------

/// Public description of a single `#[mcp_tool]` function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON-encoded JSON Schema derived from the tool's argument type via
    /// `schemars`.
    pub input_schema: String,
}

// ---- Registry ------------------------------------------------------------

/// Enumerate every `#[mcp_tool]` function compiled into this binary.
///
/// Tools are returned in registration order, which is implementation-defined
/// (linker order); callers that need a stable ordering should sort by `name`.
pub fn list_tools() -> Vec<ToolDef> {
    inventory::iter::<__private::ToolEntry>()
        .map(|entry| ToolDef {
            name: entry.name.to_string(),
            description: entry.description.to_string(),
            input_schema: (entry.schema)(),
        })
        .collect()
}

/// Dispatch a JSON-encoded call to the named tool.
///
/// Returns `Err` when no tool matches, when the args fail to deserialize, or
/// when the tool function itself returns an error.
pub fn call_tool(name: &str, args_json: &str) -> Result<String, String> {
    for entry in inventory::iter::<__private::ToolEntry>() {
        if entry.name == name {
            return (entry.dispatch)(args_json);
        }
    }
    Err(format!("unknown tool: {name}"))
}

// ---- Macro-facing private surface ---------------------------------------

#[doc(hidden)]
pub mod __private {
    //! Re-exports and helper types used by the `#[mcp_tool]` proc-macro
    //! expansion. Not part of the stable public API.

    pub use inventory;
    pub use schemars;
    pub use serde_json;

    /// Static record produced by `#[mcp_tool]` and collected by `inventory`.
    pub struct ToolEntry {
        pub name: &'static str,
        pub description: &'static str,
        /// Builds the JSON-Schema for this tool's argument type. Invoked
        /// lazily by [`super::list_tools`] (schema construction allocates).
        pub schema: fn() -> String,
        /// Deserializes JSON args, calls the user fn, serializes the return.
        pub dispatch: fn(&str) -> Result<String, String>,
    }

    inventory::collect!(ToolEntry);
}
