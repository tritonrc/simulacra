// S041 fixture: a WASM MCP module that echoes its input wrapped under
// `{ "echoed": <input> }`. Distinct from simulacra-wasm/fixtures/echo-tool.wasm
// (which returns the input as-is). The wrapping is what
// `call_tool_echo_fixture_returns_expected_json` asserts on.

wit_bindgen::generate!({
    world: "tool",
    path: "../../../../../simulacra-wasm/wit/simulacra-tool.wit",
});

struct EchoMcp;

impl Guest for EchoMcp {
    fn list_tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "echo".into(),
            description: "Echo a payload.".into(),
            input_schema: r#"{"type":"object","properties":{"query":{"type":"string"}}}"#.into(),
        }]
    }

    fn call_tool(name: String, arguments: String) -> Result<String, ToolError> {
        if name != "echo" {
            return Err(ToolError::ExecutionFailed(format!("unknown tool: {name}")));
        }
        Ok(format!(r#"{{"echoed":{arguments}}}"#))
    }
}

export!(EchoMcp);
