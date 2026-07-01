// The WIT world that real WASM MCP server modules target. Includes the
// `simulacra:mcp/http.fetch` host import — the seam through which a WASM
// module's outbound HTTP runs through Simulacra's allowlist + governance
// hooks + journal.
#[cfg(feature = "wasm")]
pub(crate) mod wit_server {
    wasmtime::component::bindgen!({
        world: "server",
        path: "wit/simulacra-mcp-server.wit",
    });
}
