wit_bindgen::generate!({
    world: "tool",
    path: "../../crates/simulacra-wasm/wit/simulacra-tool.wit",
});

struct EchoTool;

impl Guest for EchoTool {
    fn list_tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "echo".into(),
            description: "Echo back the input JSON arguments.".into(),
            input_schema: r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#.into(),
        }]
    }

    fn call_tool(name: String, arguments: String) -> Result<String, ToolError> {
        match name.as_str() {
            "echo" => Ok(arguments),
            _ => Err(ToolError::ExecutionFailed(format!("unknown tool: {name}"))),
        }
    }
}

export!(EchoTool);
