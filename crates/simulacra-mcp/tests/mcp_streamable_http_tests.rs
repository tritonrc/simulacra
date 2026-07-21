#![allow(
    clippy::type_complexity,
    clippy::await_holding_lock,
    clippy::collapsible_if,
    dead_code
)]

// Behavioral tests for S024 — MCP Streamable HTTP Transport.
//
// Tests the auto-detection handshake: try streamable HTTP POST first,
// fall back to legacy SSE on 404/405, and explicit transport selection.

include!("parts/mcp_streamable_http_tests_00.rs");
include!("parts/mcp_streamable_http_tests_01.rs");
include!("parts/mcp_streamable_http_tests_02.rs");
include!("parts/mcp_streamable_http_tests_03.rs");
include!("parts/mcp_streamable_http_tests_04.rs");
include!("parts/mcp_streamable_http_tests_05.rs");
include!("parts/mcp_streamable_http_tests_06.rs");
include!("parts/mcp_streamable_http_tests_07.rs");
include!("parts/mcp_streamable_http_tests_08.rs");
