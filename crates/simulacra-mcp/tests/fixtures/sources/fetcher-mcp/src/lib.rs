// S041 fixture: a WASM MCP module that exercises the `simulacra:mcp/http.fetch`
// host import. Its single tool, `fetch`, takes `{ "url": "<url>" }`, calls
// the host import, and returns `{ "status": <u16>, "body": "<utf-8>" }`.
//
// Used by `wasm_mcp_e2e_fetch.rs` to verify the WASM → host fetch seam end
// to end: allowlist enforcement, hook pipeline invocation, and journal
// capture all run via this fixture's `simulacra:mcp/http.fetch` call.

wit_bindgen::generate!({
    world: "server",
    path: "../../../../wit/simulacra-mcp-server.wit",
});

use simulacra::mcp::http;

struct FetcherMcp;

impl Guest for FetcherMcp {
    fn list_tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "fetch".into(),
            description: "Fetch a URL via the simulacra:mcp/http host import.".into(),
            input_schema: r#"{"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}"#.into(),
        }]
    }

    fn call_tool(name: String, arguments: String) -> Result<String, ToolError> {
        if name != "fetch" {
            return Err(ToolError::ExecutionFailed(format!("unknown tool: {name}")));
        }

        let url = match parse_url(&arguments) {
            Some(u) => u,
            None => {
                return Err(ToolError::InvalidArguments(
                    "expected JSON object with `url` string".into(),
                ));
            }
        };

        let req = http::Request {
            method: "GET".into(),
            url,
            headers: vec![("authorization".into(), "secret".into())],
            body: Vec::new(),
        };

        match http::fetch(&req) {
            Ok(resp) => {
                let body = String::from_utf8_lossy(&resp.body).into_owned();
                let body_escaped = json_string_escape(&body);
                Ok(format!(
                    r#"{{"status":{},"body":"{}"}}"#,
                    resp.status, body_escaped
                ))
            }
            Err(http::FetchError::CapabilityDenied(msg)) => Err(ToolError::ExecutionFailed(
                format!("capability_denied: {msg}"),
            )),
            Err(http::FetchError::HookDenied(msg)) => Err(ToolError::ExecutionFailed(format!(
                "hook_denied: {msg}"
            ))),
            Err(http::FetchError::Transport(msg)) => Err(ToolError::ExecutionFailed(format!(
                "transport: {msg}"
            ))),
            Err(http::FetchError::Timeout) => Err(ToolError::ExecutionFailed("timeout".into())),
        }
    }
}

fn parse_url(arguments: &str) -> Option<String> {
    let key = "\"url\"";
    let idx = arguments.find(key)?;
    let after = &arguments[idx + key.len()..];
    let colon = after.find(':')?;
    let after = &after[colon + 1..];
    let quote = after.find('"')?;
    let after = &after[quote + 1..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

fn json_string_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

export!(FetcherMcp);
