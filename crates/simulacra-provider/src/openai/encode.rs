//! One chat-completions message in the request body.

use simulacra_types::{Message, Role};

pub(super) fn message(msg: &Message) -> serde_json::Value {
    let mut m = serde_json::json!({
        "role": match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        },
        "content": msg.content,
    });
    if !msg.tool_calls.is_empty() {
        let tool_calls: Vec<serde_json::Value> = msg
            .tool_calls
            .iter()
            .map(|tc| {
                serde_json::json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments.to_string(),
                    }
                })
            })
            .collect();
        m["tool_calls"] = serde_json::Value::Array(tool_calls);
    }
    if let Some(ref tool_call_id) = msg.tool_call_id {
        m["tool_call_id"] = serde_json::Value::String(tool_call_id.clone());
    }
    m
}
