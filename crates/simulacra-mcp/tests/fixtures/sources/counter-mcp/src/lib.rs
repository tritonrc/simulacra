// S041 fixture: a WASM MCP module whose `counter` tool mutates a module-local
// `static` and returns the post-increment value. Each fresh `Store` should
// reset that state, so two consecutive calls from the runtime should both
// return `1` (proving the store is recreated per call).
//
// Reuses simulacra:tools@0.1.0 — see siblings for rationale.

use std::sync::atomic::{AtomicU64, Ordering};

wit_bindgen::generate!({
    world: "tool",
    path: "../../../../../simulacra-wasm/wit/simulacra-tool.wit",
});

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Counter;

impl Guest for Counter {
    fn list_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "counter".into(),
                description: "Increment a module-local counter and return its value.".into(),
                input_schema: r#"{"type":"object"}"#.into(),
            },
            ToolDef {
                name: "read".into(),
                description: "Read the module-local counter without mutating it.".into(),
                input_schema: r#"{"type":"object"}"#.into(),
            },
        ]
    }

    fn call_tool(name: String, _arguments: String) -> Result<String, ToolError> {
        match name.as_str() {
            "counter" => {
                let next = COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(format!("{{\"value\":{next}}}"))
            }
            "read" => Ok(format!(
                "{{\"value\":{}}}",
                COUNTER.load(Ordering::SeqCst)
            )),
            other => Err(ToolError::ExecutionFailed(format!("unknown tool: {other}"))),
        }
    }
}

export!(Counter);
