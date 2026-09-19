//! Shared fixtures for the context-strategy tests.

use crate::budget::window_tokens;
use crate::{Message, Role};

mod budget;
mod masking;
mod pinned;
mod pinned_budget;
mod pinned_exchange;
mod pinned_validity;
mod window;
mod window_unpinned;

fn msg(role: Role, content: &str) -> Message {
    Message {
        role,
        content: content.to_string(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_content: vec![],
    }
}

fn tool_msg(content: &str) -> Message {
    Message {
        role: Role::Tool,
        content: content.to_string(),
        tool_calls: Vec::new(),
        tool_call_id: Some("call_1".into()),
        provider_content: vec![],
    }
}

fn total_tokens(msgs: &[Message]) -> u64 {
    window_tokens(msgs)
}

/// The shape every hydrated turn arrives in: the system prompt, then the two
/// synthetic frames the engine rebuilds each turn and pins behind it.
fn head() -> Vec<Message> {
    vec![
        msg(Role::System, "system"),
        msg(Role::User, "<conversation-state/>"),
        msg(Role::User, "<history-window total=\"9\" hidden=\"4\">"),
    ]
}
