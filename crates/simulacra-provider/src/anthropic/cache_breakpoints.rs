//! Explicit cache breakpoints a host asks for by marking a message.
//!
//! Automatic caching writes one cache entry, at the end of each request. A
//! host that puts content changing every wake after a stable transcript marks
//! that content's message, and the request then also caches everything before
//! it, so the next wake can read the transcript back.

use simulacra_types::{Message, ProviderContentBlock};

const MARKER: &str = "cache_breakpoint_before";

/// Anthropic allows four breakpoints; automatic caching takes one.
const MAX_EXPLICIT: usize = 3;

/// Marks a message: with prompt caching on, the request caches the prefix that
/// ends just before it. A marked system message is ignored: system content
/// leads every request. Other providers ignore it.
pub fn cache_breakpoint_before() -> ProviderContentBlock {
    ProviderContentBlock {
        provider: "anthropic".into(),
        value: serde_json::json!({ "type": MARKER }),
    }
}

pub(crate) fn is_marked(message: &Message) -> bool {
    message.provider_content.iter().any(is_marker)
}

/// True when `blocks` carries nothing but markers, which never reach the wire.
pub(crate) fn only_markers(blocks: &[ProviderContentBlock]) -> bool {
    blocks.iter().all(is_marker)
}

fn is_marker(block: &ProviderContentBlock) -> bool {
    block.provider == "anthropic"
        && block.value.get("type").and_then(|t| t.as_str()) == Some(MARKER)
}

/// Puts `cache_control` on the last cacheable block of each listed message in
/// a serialized request. Thinking blocks and empty text cannot carry it.
pub(crate) fn apply(body: &mut serde_json::Value, messages: &[usize]) {
    let skip = messages.len().saturating_sub(MAX_EXPLICIT);
    for &index in &messages[skip..] {
        let Some(content) = body
            .get_mut("messages")
            .and_then(|m| m.get_mut(index))
            .and_then(|m| m.get_mut("content"))
        else {
            continue;
        };
        if let Some(text) = content.as_str() {
            if text.is_empty() {
                continue;
            }
            *content = serde_json::json!([{ "type": "text", "text": text }]);
        }
        let Some(blocks) = content.as_array_mut() else {
            continue;
        };
        if let Some(block) = blocks.iter_mut().rev().find(|block| cacheable(block)) {
            block["cache_control"] = serde_json::json!({ "type": "ephemeral" });
        }
    }
}

fn cacheable(block: &serde_json::Value) -> bool {
    match block.get("type").and_then(|t| t.as_str()) {
        Some("thinking" | "redacted_thinking") => false,
        Some("text") => block
            .get("text")
            .and_then(|t| t.as_str())
            .is_some_and(|t| !t.is_empty()),
        _ => true,
    }
}

#[cfg(test)]
#[path = "cache_breakpoints_tests.rs"]
mod tests;
