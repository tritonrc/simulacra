wit_bindgen::generate!({
    world: "tool",
    path: "../../crates/simulacra-wasm/wit/simulacra-tool.wit",
});

struct SandboxTestTool;

impl Guest for SandboxTestTool {
    fn list_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "read_file".into(),
                description: "Read a file and return its contents.".into(),
                input_schema: r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#.into(),
            },
            ToolDef {
                name: "write_file".into(),
                description: "Write content to a file.".into(),
                input_schema: r#"{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}"#.into(),
            },
            ToolDef {
                name: "read_env".into(),
                description: "Read an environment variable.".into(),
                input_schema: r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}"#.into(),
            },
        ]
    }

    fn call_tool(name: String, arguments: String) -> Result<String, ToolError> {
        match name.as_str() {
            "read_file" => {
                let path = extract_string_field(&arguments, "path")
                    .ok_or_else(|| ToolError::InvalidArguments("missing 'path'".into()))?;
                match std::fs::read_to_string(&path) {
                    Ok(content) => Ok(format!(r#"{{"content":{}}}"#, json_escape(&content))),
                    Err(e) => Err(ToolError::ExecutionFailed(format!("read failed: {e}"))),
                }
            }
            "write_file" => {
                let path = extract_string_field(&arguments, "path")
                    .ok_or_else(|| ToolError::InvalidArguments("missing 'path'".into()))?;
                let content = extract_string_field(&arguments, "content")
                    .ok_or_else(|| ToolError::InvalidArguments("missing 'content'".into()))?;
                match std::fs::write(&path, &content) {
                    Ok(()) => Ok(r#"{"written":true}"#.into()),
                    Err(e) => Err(ToolError::ExecutionFailed(format!("write failed: {e}"))),
                }
            }
            "read_env" => {
                let var_name = extract_string_field(&arguments, "name")
                    .ok_or_else(|| ToolError::InvalidArguments("missing 'name'".into()))?;
                let value = std::env::var(&var_name).unwrap_or_default();
                Ok(format!(r#"{{"value":{}}}"#, json_escape(&value)))
            }
            _ => Err(ToolError::ExecutionFailed(format!("unknown tool: {name}"))),
        }
    }
}

fn extract_string_field(json: &str, field: &str) -> Option<String> {
    let pattern = format!(r#""{}":""#, field);
    let start = json.find(&pattern)? + pattern.len();
    let rest = &json[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn json_escape(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{}\"", escaped)
}

export!(SandboxTestTool);
