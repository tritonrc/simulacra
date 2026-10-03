//! Tool-result images on the chat-completions wire.
//!
//! A `tool` message carries text only, so images a tool returned travel in
//! one `user` message placed after the whole run of tool messages that
//! answers an assistant turn (or joins the user message that follows it).
//! Placing it inside the run would break the call/result pairing.

use simulacra_types::{Message, ProviderContentBlock, Role};

use super::encode;

/// Encode a normalized message list, adding one image-carrying `user`
/// message after each run of tool messages whose results carried images.
pub(super) fn encode_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(messages.len());
    let mut parts: Vec<serde_json::Value> = Vec::new();
    for msg in messages {
        if msg.role == Role::User && !parts.is_empty() {
            // Folded into the user message that follows: some chat templates
            // reject two user messages in a row.
            if !msg.content.is_empty() {
                parts.push(serde_json::json!({"type": "text", "text": msg.content}));
            }
            flush(&mut out, &mut parts);
            continue;
        }
        if msg.role != Role::Tool {
            flush(&mut out, &mut parts);
        }
        out.push(encode::message(msg));
        if msg.role == Role::Tool {
            collect(msg, &mut parts);
        }
    }
    flush(&mut out, &mut parts);
    out
}

fn flush(out: &mut Vec<serde_json::Value>, parts: &mut Vec<serde_json::Value>) {
    if !parts.is_empty() {
        out.push(serde_json::json!({
            "role": "user",
            "content": std::mem::take(parts),
        }));
    }
}

fn collect(msg: &Message, parts: &mut Vec<serde_json::Value>) {
    let images: Vec<&ProviderContentBlock> = msg
        .provider_content
        .iter()
        .filter(|b| b.provider == "anthropic")
        .filter(|b| b.value.get("type").and_then(|t| t.as_str()) == Some("image"))
        .collect();
    if images.is_empty() {
        return;
    }
    let id = msg.tool_call_id.as_deref().unwrap_or_default();
    parts.push(serde_json::json!({
        "type": "text",
        "text": format!("Image(s) returned by tool call {id}:"),
    }));
    for block in images {
        parts.push(image_part(block.value.get("source")));
    }
}

fn image_part(source: Option<&serde_json::Value>) -> serde_json::Value {
    let field = |k: &str| source?.get(k)?.as_str();
    let url = match field("type") {
        Some("base64") => match (field("media_type"), field("data")) {
            (Some(media), Some(data)) => Some(format!("data:{media};base64,{data}")),
            _ => None,
        },
        Some("url") => field("url").map(str::to_owned),
        _ => None,
    };
    match url {
        // `high` bounds an image's cost on every model (at most 2,500
        // patches); `auto` means unbounded `original` on some.
        Some(url) => serde_json::json!({
            "type": "image_url",
            "image_url": {"url": url, "detail": "high"},
        }),
        None => {
            tracing::warn!("tool-result image has a source this provider cannot send");
            serde_json::json!({
                "type": "text",
                "text": "[An image returned by the tool is unavailable on this provider.]",
            })
        }
    }
}
