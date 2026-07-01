use opentelemetry::metrics::{Counter, Histogram};

/// Lazily-initialized OTel meter instruments for MCP tool calls.
/// Created on first use so they pick up the global MeterProvider
/// (which may not be set at construction time).
pub(crate) struct McpMeters {
    pub(crate) tool_duration: Histogram<f64>,
    /// S010: `simulacra.mcp.calls` counter with labels `server`, `tool`.
    pub(crate) calls: Counter<u64>,
    pub(crate) tool_errors: Counter<u64>,
    /// S024: `simulacra.mcp.session_expired` counter with label `server`.
    pub(crate) session_expired: Counter<u64>,
    /// S041 §Observability: `simulacra.wasm.fuel_consumed` histogram with
    /// labels `module` and `tool`. Recorded per WASM MCP tool call —
    /// the OTel meter bridge picks it up regardless of the
    /// tracing-fields histogram convention being supported downstream.
    #[cfg(feature = "wasm")]
    pub(crate) wasm_fuel_consumed: Histogram<u64>,
}

impl McpMeters {
    pub(crate) fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<McpMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-mcp");
            McpMeters {
                tool_duration: meter
                    .f64_histogram("simulacra.mcp.tool.duration")
                    .with_unit("ms")
                    .with_description("MCP tool call duration")
                    .build(),
                calls: meter
                    .u64_counter("simulacra.mcp.calls")
                    .with_description("Total MCP tool calls")
                    .build(),
                tool_errors: meter
                    .u64_counter("simulacra.mcp.tool.errors")
                    .with_description("Total MCP tool call errors")
                    .build(),
                session_expired: meter
                    .u64_counter("simulacra.mcp.session_expired")
                    .with_description("MCP session expiry events")
                    .build(),
                #[cfg(feature = "wasm")]
                wasm_fuel_consumed: meter
                    .u64_histogram("simulacra.wasm.fuel_consumed")
                    .with_description("Wasmtime fuel consumed per WASM MCP tool call")
                    .build(),
            }
        })
    }
}
