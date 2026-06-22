// S041 fixture: a WASM MCP module whose `burn_fuel` tool runs an unbounded
// loop. Wasmtime's fuel metering will trap it as `Trap::OutOfFuel`, which
// the runtime should surface as `ToolError::ExecutionFailed("fuel exhausted")`.
//
// Reuses simulacra:tools@0.1.0 — the runtime stub does not yet bind a different
// world for MCP.

wit_bindgen::generate!({
    world: "tool",
    path: "../../../../../simulacra-wasm/wit/simulacra-tool.wit",
});

struct BurnFuel;

impl Guest for BurnFuel {
    fn list_tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "burn_fuel".into(),
            description: "Loops forever; expected to trap on Trap::OutOfFuel.".into(),
            input_schema: r#"{"type":"object","properties":{"iterations":{"type":"integer"}}}"#.into(),
        }]
    }

    fn call_tool(name: String, _arguments: String) -> Result<String, ToolError> {
        if name != "burn_fuel" {
            return Err(ToolError::ExecutionFailed(format!("unknown tool: {name}")));
        }
        // Force an unbounded compute load. `std::hint::black_box` keeps the
        // optimizer from collapsing the loop. Wasmtime's fuel meter traps.
        let mut acc = 0u64;
        loop {
            acc = std::hint::black_box(acc.wrapping_add(1));
            if acc == u64::MAX {
                // Unreachable in practice — present to keep the type-checker happy.
                return Ok("0".into());
            }
        }
    }
}

export!(BurnFuel);
