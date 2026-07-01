// End-to-end test that drives the full WASM -> host fetch seam:
// a WASM MCP module (`fetcher-mcp.wasm`) calls `simulacra:mcp/http.fetch`
// through `wit_server::Server::call_call_tool`, which dispatches into
// the host-side `wasm_mcp_fetch`. This exercises the real allowlist
// enforcement, the real `simulacra_hooks::HookPipeline` (`Phase::Before` /
// `Phase::After`), and the real journal append path -- all driven by
// a real WASIp2 component, not by host-side helpers.

include!("parts/wasm_mcp_e2e_fetch_00.rs");
include!("parts/wasm_mcp_e2e_fetch_01.rs");
include!("parts/wasm_mcp_e2e_fetch_02.rs");
include!("parts/wasm_mcp_e2e_fetch_03.rs");
