//! Sample `#[mcp_tool]` functions used by the SDK's own test suite (and as
//! ready-to-copy examples for new authors).
//!
//! These are always compiled — including in published builds — because the
//! `inventory` crate populates the registry at link time and we want the
//! macro tests in `tests/macro.rs` to find tools without the test crate
//! having to redefine them. Authors building real WASM MCP servers add their
//! own tools in their own crates and register their own
//! `#[mcp_tool]`-annotated functions; the fixtures here do not interfere
//! because the runtime only enumerates whatever has actually been linked in.

use crate::mcp_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Echo argument struct.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct EchoArgs {
    pub query: String,
}

/// Echo response — wraps the original args under an `echoed` field so the
/// dispatch test can confirm the function actually ran.
#[derive(Debug, Serialize)]
pub struct EchoOut {
    pub echoed: EchoArgs,
}

#[mcp_tool(description = "Echo the input back as JSON")]
fn echo(args: EchoArgs) -> Result<EchoOut, String> {
    Ok(EchoOut { echoed: args })
}

/// Args for the fetch helper fixture.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchHelperArgs {
    pub url: String,
}

/// `fetch_helper` exercises the SDK's `fetch::*` helpers. The body of the
/// returned `Response` documents the routing path so callers can verify the
/// tool went through the imported `simulacra:mcp/http.fetch` interface rather
/// than reaching out directly.
#[mcp_tool(description = "Fetch a URL via the imported simulacra:mcp/http.fetch helper")]
fn fetch_helper(args: FetchHelperArgs) -> Result<crate::fetch::Response, String> {
    crate::fetch::get(args.url, &[]).map_err(|e| e.to_string())
}
