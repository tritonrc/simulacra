//! Putting each tool result straight after the assistant message that called
//! it, keeping the latest result per call and dropping orphans. Persisted
//! histories can hold late or repeated results (a paused call answered
//! later, then again on approval), and both Anthropic and OpenAI-compatible
//! APIs reject a request whose tool messages are out of place.

use std::collections::{HashMap, HashSet};

use simulacra_types::{Message, Role};

pub(crate) fn normalize_tool_pairs(messages: &[Message]) -> Vec<Message> {
    let mut normalized = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        match message.role {
            Role::Tool => {
                // Tool results are injected immediately after their assistant
                // tool_use anchor below. Unknown and malformed ids are dropped
                // as orphans.
            }
            _ => {
                normalized.push(message.clone());

                if message.role != Role::Assistant || message.tool_calls.is_empty() {
                    continue;
                }

                let expected_tool_use_ids: HashSet<&str> = message
                    .tool_calls
                    .iter()
                    .map(|tool_call| tool_call.id.as_str())
                    .collect();
                let mut latest_tool_results: HashMap<&str, Message> = HashMap::new();

                for candidate in messages
                    .iter()
                    .skip(index + 1)
                    .take_while(|candidate| candidate.role != Role::Assistant)
                {
                    if candidate.role != Role::Tool {
                        continue;
                    }

                    let Some(tool_call_id) = candidate.tool_call_id.as_deref() else {
                        continue;
                    };

                    if expected_tool_use_ids.contains(tool_call_id) {
                        latest_tool_results.insert(tool_call_id, candidate.clone());
                    }
                }

                for tool_call in &message.tool_calls {
                    if let Some(tool_result) = latest_tool_results.remove(tool_call.id.as_str()) {
                        normalized.push(tool_result);
                    } else {
                        tracing::warn!(
                            tool_use_id = %tool_call.id,
                            content_preview = %message.content.chars().take(120).collect::<String>(),
                            "assistant tool call has no matching tool result before the next assistant message; the provider will likely reject the request"
                        );
                    }
                }
            }
        }
    }

    normalized
}
