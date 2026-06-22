// S041 fixture: a WASM MCP module that exports multiple tools to exercise
// `module_exporting_multiple_tools_registers_multiple_tool_definitions`.
//
// Reuses the simulacra:tools@0.1.0 WIT shape since S041 spec §Design notes that
// the `types` interface is identical to S025's. Phase 1c only requires
// loadable component bytes — the runtime is still stubbed.

wit_bindgen::generate!({
    world: "tool",
    path: "../../../../../simulacra-wasm/wit/simulacra-tool.wit",
});

struct MultiTool;

impl Guest for MultiTool {
    fn list_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "echo".into(),
                description: "Echo a payload.".into(),
                input_schema: r#"{"type":"object","properties":{"query":{"type":"string"}}}"#
                    .into(),
            },
            ToolDef {
                name: "reverse".into(),
                description: "Reverse a string payload.".into(),
                input_schema: r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#.into(),
            },
        ]
    }

    fn call_tool(name: String, arguments: String) -> Result<String, ToolError> {
        match name.as_str() {
            "echo" => Ok(arguments),
            // Naive byte-reverse — adequate for the structural test surface.
            "reverse" => Ok(arguments.chars().rev().collect::<String>()),
            other => Err(ToolError::ExecutionFailed(format!("unknown tool: {other}"))),
        }
    }
}

export!(MultiTool);
