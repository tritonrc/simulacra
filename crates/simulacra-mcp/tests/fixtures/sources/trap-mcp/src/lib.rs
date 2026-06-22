// S041 fixture: a WASM MCP module whose `trap` tool always traps. Used by
// `tracing_error_emitted_on_wasm_trap_during_call_tool` to verify that the
// runtime turns wasmtime traps into ERROR-level logs.
//
// `unreachable!()` lowers to a `wasm32::unreachable` instruction, which
// wasmtime surfaces as `Trap::UnreachableCodeReached`.

wit_bindgen::generate!({
    world: "tool",
    path: "../../../../../simulacra-wasm/wit/simulacra-tool.wit",
});

struct TrapTool;

impl Guest for TrapTool {
    fn list_tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "trap".into(),
            description: "Always traps via the wasm `unreachable` instruction.".into(),
            input_schema: r#"{"type":"object"}"#.into(),
        }]
    }

    fn call_tool(name: String, _arguments: String) -> Result<String, ToolError> {
        if name == "trap" {
            // Lowers to a `wasm32::unreachable` instruction.
            unreachable!("S041 trap fixture deliberately panics");
        }
        Err(ToolError::ExecutionFailed(format!("unknown tool: {name}")))
    }
}

export!(TrapTool);
