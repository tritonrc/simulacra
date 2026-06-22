use simulacra_mcp_server_sdk::{call_tool, list_tools};

#[test]
#[ignore = "requires S041 #[mcp_tool] proc-macro + wasm32-wasip2 toolchain"]
fn mcp_tool_attribute_compiles_to_wasip2_component() {
    let tools = list_tools();
    assert!(
        !tools.is_empty(),
        "#[mcp_tool] crates should expose component metadata once the SDK macro exists"
    );
}

#[test]
fn list_tools_returns_one_entry_per_mcp_tool_function() {
    let tools = list_tools();
    assert_eq!(
        tools.len(),
        2,
        "list_tools should return one ToolDef per #[mcp_tool] function"
    );
}

#[test]
fn input_schema_is_derived_from_arg_type_and_parses_as_json_schema() {
    let tools = list_tools();
    let schema = serde_json::from_str::<serde_json::Value>(&tools[0].input_schema)
        .expect("input_schema should parse as JSON");

    assert!(
        schema.is_object(),
        "schemars-derived input_schema should be valid JSON Schema"
    );
}

#[test]
fn call_tool_dispatches_to_function_and_serializes_return() {
    let output = call_tool("echo", r#"{"query":"simulacra"}"#)
        .expect("call_tool should dispatch to the named function");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output).expect("tool result should be JSON"),
        serde_json::json!({ "echoed": { "query": "simulacra" } })
    );
}

#[test]
fn fetch_helpers_route_through_imported_simulacra_mcp_http_fetch() {
    let output = call_tool("fetch_helper", r#"{"url":"https://example.com"}"#)
        .expect("fetch helper should dispatch through call_tool");

    assert!(
        output.contains("simulacra:mcp/http.fetch"),
        "SDK fetch helpers should route through the imported simulacra:mcp/http.fetch interface"
    );
}
