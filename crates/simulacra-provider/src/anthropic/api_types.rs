//! Serde models for the Anthropic Messages API.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ── Request types ──────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(crate) struct ApiRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ApiTool<'a>>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ApiMessage {
    pub role: String,
    pub content: ApiMessageContent,
}

/// Message content can be a plain string or an array of content blocks.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ApiMessageContent {
    Text(String),
    Blocks(Vec<ApiRequestContentBlock>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub(crate) enum ApiRequestContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
}

#[derive(Debug, Serialize)]
pub(crate) struct ApiTool<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub input_schema: &'a serde_json::Value,
}

// ── Response types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct ApiResponse {
    pub id: String,
    pub content: Vec<ApiResponseContentBlock>,
    pub model: String,
    pub stop_reason: Option<String>,
    pub usage: ApiUsage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum ApiResponseContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "thinking")]
    Thinking {
        #[allow(dead_code)]
        thinking: String,
        #[allow(dead_code)]
        signature: Option<String>,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking {
        #[allow(dead_code)]
        data: String,
    },
}

#[derive(Debug, Deserialize)]
pub(crate) struct ApiUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

// ── Error response ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct ApiErrorResponse {
    pub error: ApiErrorDetail,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ApiErrorDetail {
    /// Anthropic error type (e.g. "authentication_error", "rate_limit_error").
    /// Reserved for future fine-grained error classification.
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub error_type: String,
    pub message: String,
}

// ── Conversions ────────────────────────────────────────────────────

use simulacra_types::{
    FinishReason, Message, ProviderContentBlock, ProviderResponse, Role, TokenUsage,
    ToolCallMessage, ToolDefinition,
};

fn normalize_tool_pairs(messages: &[Message]) -> Vec<Message> {
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
                            "assistant tool_use has no matching tool_result before the next assistant message; Anthropic will likely reject the request"
                        );
                    }
                }
            }
        }
    }

    normalized
}

fn message_sequence_changed(original: &[Message], normalized: &[Message]) -> bool {
    if original.len() != normalized.len() {
        return true;
    }

    original
        .iter()
        .zip(normalized.iter())
        .any(|(left, right)| !messages_equal(left, right))
}

fn messages_equal(left: &Message, right: &Message) -> bool {
    // Provider-native content is part of message identity because Anthropic
    // requires thinking blocks to be returned verbatim in tool-use transcripts.
    left.role == right.role
        && left.content == right.content
        && left.tool_call_id == right.tool_call_id
        && left.provider_content == right.provider_content
        && left.tool_calls.len() == right.tool_calls.len()
        && left
            .tool_calls
            .iter()
            .zip(right.tool_calls.iter())
            .all(|(left_call, right_call)| {
                left_call.id == right_call.id
                    && left_call.name == right_call.name
                    && left_call.arguments == right_call.arguments
            })
}

fn anthropic_provider_blocks(
    provider_content: &[ProviderContentBlock],
) -> Vec<ApiRequestContentBlock> {
    provider_content
        .iter()
        .filter(|block| block.provider == "anthropic")
        .filter_map(
            |block| match block.value.get("type").and_then(|value| value.as_str()) {
                Some("thinking") => {
                    let signature = block
                        .value
                        .get("signature")
                        .and_then(|value| value.as_str())
                        .map(ToString::to_string);
                    if signature.is_none() {
                        tracing::warn!(
                            "Anthropic thinking block is missing a signature; Anthropic will likely reject the continued request"
                        );
                    }
                    Some(ApiRequestContentBlock::Thinking {
                        thinking: block
                            .value
                            .get("thinking")
                            .and_then(|value| value.as_str())
                            .unwrap_or("")
                            .to_string(),
                        signature,
                    })
                }
                Some("redacted_thinking") => Some(ApiRequestContentBlock::RedactedThinking {
                    data: block
                        .value
                        .get("data")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string(),
                }),
                _ => None,
            },
        )
        .collect()
}

fn tool_result_count(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .count()
}

/// Convert simulacra Messages into Anthropic API messages, extracting system messages separately.
/// `max_tokens` should be derived from the remaining budget by the caller.
pub(crate) fn build_request_parts<'a>(
    messages: &'a [Message],
    tools: &'a [ToolDefinition],
    model: &'a str,
    max_tokens: u32,
) -> ApiRequest<'a> {
    let normalized = normalize_tool_pairs(messages);
    if message_sequence_changed(messages, &normalized) {
        let original_tool_results = tool_result_count(messages);
        let normalized_tool_results = tool_result_count(&normalized);
        tracing::debug!(
            original_tool_results,
            normalized_tool_results,
            "normalized Anthropic tool_result sequence"
        );
    }

    let mut system_text: Option<String> = None;
    let mut api_messages: Vec<ApiMessage> = Vec::new();

    for msg in &normalized {
        match msg.role {
            Role::System => {
                // Anthropic takes system as a top-level field, not in messages.
                // Concatenate if multiple system messages exist (defensive).
                system_text = Some(match system_text {
                    Some(existing) => format!("{existing}\n\n{}", msg.content),
                    None => msg.content.clone(),
                });
            }
            Role::User => {
                api_messages.push(ApiMessage {
                    role: "user".into(),
                    content: ApiMessageContent::Text(msg.content.clone()),
                });
            }
            Role::Assistant => {
                // Build content blocks: text + tool_use (required for multi-turn tool conversations)
                let mut blocks: Vec<ApiRequestContentBlock> =
                    anthropic_provider_blocks(&msg.provider_content);
                if !msg.content.is_empty() {
                    blocks.push(ApiRequestContentBlock::Text {
                        text: msg.content.clone(),
                    });
                }
                for tc in &msg.tool_calls {
                    blocks.push(ApiRequestContentBlock::ToolUse {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        input: tc.arguments.clone(),
                    });
                }
                let content = if msg.provider_content.is_empty()
                    && blocks.len() == 1
                    && msg.tool_calls.is_empty()
                {
                    ApiMessageContent::Text(msg.content.clone())
                } else if blocks.is_empty() {
                    // Assistant messages must have some content
                    ApiMessageContent::Text(msg.content.clone())
                } else {
                    ApiMessageContent::Blocks(blocks)
                };
                api_messages.push(ApiMessage {
                    role: "assistant".into(),
                    content,
                });
            }
            Role::Tool => {
                // Tool results go as user messages with tool_result content blocks.
                let Some(tool_call_id) = msg.tool_call_id.clone() else {
                    continue;
                };
                api_messages.push(ApiMessage {
                    role: "user".into(),
                    content: ApiMessageContent::Blocks(vec![ApiRequestContentBlock::ToolResult {
                        tool_use_id: tool_call_id,
                        content: msg.content.clone(),
                        is_error: false,
                    }]),
                });
            }
        }
    }

    let api_tools: Vec<ApiTool<'_>> = tools
        .iter()
        .map(|t| ApiTool {
            name: &t.name,
            description: &t.description,
            input_schema: &t.input_schema,
        })
        .collect();

    ApiRequest {
        model,
        max_tokens,
        messages: api_messages,
        system: system_text,
        tools: api_tools,
    }
}

/// Convert an Anthropic API response into a simulacra ProviderResponse.
pub(crate) fn into_provider_response(resp: ApiResponse) -> ProviderResponse {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut provider_content = Vec::new();

    for block in &resp.content {
        match block {
            ApiResponseContentBlock::Text { text } => {
                text_parts.push(text.clone());
            }
            ApiResponseContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCallMessage {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: input.clone(),
                });
            }
            ApiResponseContentBlock::Thinking {
                thinking,
                signature,
            } => {
                provider_content.push(ProviderContentBlock {
                    provider: "anthropic".to_string(),
                    value: serde_json::json!({
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": signature,
                    }),
                });
            }
            ApiResponseContentBlock::RedactedThinking { data } => {
                provider_content.push(ProviderContentBlock {
                    provider: "anthropic".to_string(),
                    value: serde_json::json!({
                        "type": "redacted_thinking",
                        "data": data,
                    }),
                });
            }
        }
    }

    let finish_reason = match resp.stop_reason.as_deref() {
        Some("tool_use") => FinishReason::ToolUse,
        Some("max_tokens") => FinishReason::MaxTokens,
        Some("stop_sequence") => FinishReason::StopSequence,
        _ => FinishReason::EndTurn,
    };

    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: text_parts.join(""),
            tool_calls,
            tool_call_id: None,
            provider_content,
        },
        token_usage: TokenUsage {
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
        },
        finish_reason,
        provider_response_id: Some(resp.id),
        model: resp.model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant(content: &str, tool_call_ids: &[&str]) -> Message {
        Message {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: tool_call_ids
                .iter()
                .map(|id| ToolCallMessage {
                    id: (*id).into(),
                    name: format!("tool_{id}"),
                    arguments: json!({ "id": id }),
                })
                .collect(),
            tool_call_id: None,
            provider_content: vec![],
        }
    }

    fn user(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }
    }

    fn tool(tool_call_id: &str, content: &str) -> Message {
        Message {
            role: Role::Tool,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: Some(tool_call_id.into()),
            provider_content: vec![],
        }
    }

    fn malformed_tool(content: &str) -> Message {
        Message {
            role: Role::Tool,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }
    }

    fn assert_message(message: &Message, role: Role, content: &str, tool_call_id: Option<&str>) {
        assert_eq!(message.role, role);
        assert_eq!(message.content, content);
        assert_eq!(message.tool_call_id.as_deref(), tool_call_id);
    }

    fn tool_result_id(api_message: &ApiMessage) -> Option<&str> {
        let ApiMessageContent::Blocks(blocks) = &api_message.content else {
            return None;
        };
        blocks.iter().find_map(|block| {
            if let ApiRequestContentBlock::ToolResult { tool_use_id, .. } = block {
                Some(tool_use_id.as_str())
            } else {
                None
            }
        })
    }

    fn tool_result_content(api_message: &ApiMessage) -> Option<&str> {
        let ApiMessageContent::Blocks(blocks) = &api_message.content else {
            return None;
        };
        blocks.iter().find_map(|block| {
            if let ApiRequestContentBlock::ToolResult { content, .. } = block {
                Some(content.as_str())
            } else {
                None
            }
        })
    }

    fn has_tool_use(api_message: &ApiMessage, id: &str) -> bool {
        let ApiMessageContent::Blocks(blocks) = &api_message.content else {
            return false;
        };
        blocks.iter().any(|block| {
            matches!(
                block,
                ApiRequestContentBlock::ToolUse { id: tool_use_id, .. } if tool_use_id == id
            )
        })
    }

    fn has_thinking(api_message: &ApiMessage, signature: &str) -> bool {
        let ApiMessageContent::Blocks(blocks) = &api_message.content else {
            return false;
        };
        blocks.iter().any(|block| {
            matches!(
                block,
                ApiRequestContentBlock::Thinking {
                    signature: Some(block_signature),
                    ..
                } if block_signature == signature
            )
        })
    }

    fn has_unsigned_thinking(api_message: &ApiMessage) -> bool {
        let ApiMessageContent::Blocks(blocks) = &api_message.content else {
            return false;
        };
        blocks.iter().any(|block| {
            matches!(
                block,
                ApiRequestContentBlock::Thinking {
                    signature: None,
                    ..
                }
            )
        })
    }

    fn has_redacted_thinking(api_message: &ApiMessage, data: &str) -> bool {
        let ApiMessageContent::Blocks(blocks) = &api_message.content else {
            return false;
        };
        blocks.iter().any(|block| {
            matches!(
                block,
                ApiRequestContentBlock::RedactedThinking { data: block_data } if block_data == data
            )
        })
    }

    fn assert_same_messages(actual: &[Message], expected: &[Message]) {
        let actual = serde_json::to_vec(actual).expect("actual messages should serialize");
        let expected = serde_json::to_vec(expected).expect("expected messages should serialize");
        assert_eq!(actual, expected);
    }

    #[derive(Clone, Default)]
    struct CapturedEvents(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl<S> tracing_subscriber::Layer<S> for CapturedEvents
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor<'a> {
                fields: &'a mut Vec<String>,
            }

            impl tracing::field::Visit for Visitor<'_> {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.fields.push(format!("{}={value:?}", field.name()));
                }
            }

            if *event.metadata().level() != tracing::Level::WARN {
                return;
            }

            let mut fields = Vec::new();
            event.record(&mut Visitor {
                fields: &mut fields,
            });
            self.0
                .lock()
                .expect("captured events mutex poisoned")
                .push(fields.into_iter().collect::<Vec<_>>().join(" "));
        }
    }

    #[test]
    fn normalize_dedups_and_reorders_resume_sequence() {
        let messages = vec![
            assistant("", &["X"]),
            tool("X", "paused"),
            assistant("evidence", &[]),
            user("ship it"),
            tool("X", "real"),
        ];

        let normalized = normalize_tool_pairs(&messages);

        assert_eq!(normalized.len(), 4);
        assert_message(&normalized[0], Role::Assistant, "", None);
        assert_eq!(normalized[0].tool_calls[0].id, "X");
        assert_message(&normalized[1], Role::Tool, "paused", Some("X"));
        assert_message(&normalized[2], Role::Assistant, "evidence", None);
        assert_message(&normalized[3], Role::User, "ship it", None);
    }

    #[test]
    fn normalize_dedups_tool_results_per_assistant_turn() {
        let messages = vec![
            assistant("first", &["X"]),
            tool("X", "first result"),
            assistant("second", &["X"]),
            tool("X", "second result"),
            user("done"),
        ];

        let normalized = normalize_tool_pairs(&messages);

        assert_eq!(normalized.len(), 5);
        assert_message(&normalized[0], Role::Assistant, "first", None);
        assert_message(&normalized[1], Role::Tool, "first result", Some("X"));
        assert_message(&normalized[2], Role::Assistant, "second", None);
        assert_message(&normalized[3], Role::Tool, "second result", Some("X"));
        assert_message(&normalized[4], Role::User, "done", None);
    }

    #[test]
    fn normalize_drops_orphan_tool_result() {
        let messages = vec![user("hello"), tool("orphan", "no matching call")];

        let normalized = normalize_tool_pairs(&messages);

        assert_eq!(normalized.len(), 1);
        assert_message(&normalized[0], Role::User, "hello", None);
    }

    #[test]
    fn normalize_drops_malformed_tool_result_without_tool_call_id() {
        let messages = vec![user("hello"), malformed_tool("missing id")];

        let normalized = normalize_tool_pairs(&messages);

        assert_eq!(normalized.len(), 1);
        assert_message(&normalized[0], Role::User, "hello", None);
    }

    #[test]
    fn normalize_places_multiple_tool_results_after_their_assistant() {
        let messages = vec![
            assistant("needs tools", &["A", "B"]),
            user("later"),
            tool("B", "result b"),
            tool("A", "result a"),
        ];

        let normalized = normalize_tool_pairs(&messages);

        assert_eq!(normalized.len(), 4);
        assert_message(&normalized[0], Role::Assistant, "needs tools", None);
        assert_message(&normalized[1], Role::Tool, "result a", Some("A"));
        assert_message(&normalized[2], Role::Tool, "result b", Some("B"));
        assert_message(&normalized[3], Role::User, "later", None);
    }

    #[test]
    fn normalize_is_identity_for_valid_transcript() {
        let messages = vec![
            assistant("needs tool", &["A"]),
            tool("A", "result"),
            user("done"),
        ];

        let normalized = normalize_tool_pairs(&messages);

        assert_eq!(normalized.len(), messages.len());
        for (actual, expected) in normalized.iter().zip(messages.iter()) {
            assert_message(
                actual,
                expected.role.clone(),
                &expected.content,
                expected.tool_call_id.as_deref(),
            );
            assert_eq!(actual.tool_calls.len(), expected.tool_calls.len());
        }
    }

    #[test]
    fn normalize_is_identity_for_valid_two_turn_multi_tool_transcript() {
        let messages = vec![
            assistant("first", &["A"]),
            tool("A", "result a"),
            assistant("second", &["B"]),
            tool("B", "result b"),
            user("done"),
        ];

        let normalized = normalize_tool_pairs(&messages);

        assert_same_messages(&normalized, &messages);
    }

    #[test]
    fn build_request_parts_preserves_anthropic_thinking_blocks_on_assistant_tool_use() {
        let mut assistant = assistant("use tool", &["X"]);
        assistant.provider_content = vec![
            ProviderContentBlock {
                provider: "anthropic".to_string(),
                value: json!({
                    "type": "thinking",
                    "thinking": "summary",
                    "signature": "sig-123"
                }),
            },
            ProviderContentBlock {
                provider: "anthropic".to_string(),
                value: json!({
                    "type": "redacted_thinking",
                    "data": "encrypted"
                }),
            },
        ];
        let messages = vec![assistant, tool("X", "result")];

        let request = build_request_parts(&messages, &[], "claude-fable-5", 1024);

        assert_eq!(request.messages.len(), 2);
        assert!(has_thinking(&request.messages[0], "sig-123"));
        assert!(has_redacted_thinking(&request.messages[0], "encrypted"));
        assert!(has_tool_use(&request.messages[0], "X"));
        assert_eq!(tool_result_id(&request.messages[1]), Some("X"));
    }

    #[test]
    fn build_request_parts_uses_blocks_for_provider_only_assistant_content() {
        let assistant = Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![ProviderContentBlock {
                provider: "anthropic".to_string(),
                value: json!({
                    "type": "thinking",
                    "thinking": "",
                    "signature": "sig-only"
                }),
            }],
        };

        let messages = vec![assistant];
        let request = build_request_parts(&messages, &[], "claude-fable-5", 1024);

        assert_eq!(request.messages.len(), 1);
        assert!(has_thinking(&request.messages[0], "sig-only"));
    }

    #[test]
    fn build_request_parts_warns_for_unsigned_anthropic_thinking_blocks() {
        use tracing_subscriber::prelude::*;

        let assistant = Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![ProviderContentBlock {
                provider: "anthropic".to_string(),
                value: json!({
                    "type": "thinking",
                    "thinking": "unsigned"
                }),
            }],
        };
        let captured = CapturedEvents::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let messages = vec![assistant];

        let request = tracing::subscriber::with_default(subscriber, || {
            build_request_parts(&messages, &[], "claude-fable-5", 1024)
        });

        assert!(has_unsigned_thinking(&request.messages[0]));
        let events = captured.0.lock().expect("captured events mutex poisoned");
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("thinking block is missing a signature"));
    }

    #[test]
    fn build_request_parts_warns_for_unmatched_assistant_tool_use() {
        use tracing_subscriber::prelude::*;

        let captured = CapturedEvents::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let messages = vec![assistant("need the missing value", &["missing"])];

        tracing::subscriber::with_default(subscriber, || {
            let _request = build_request_parts(&messages, &[], "claude-test", 1024);
        });

        let events = captured.0.lock().expect("captured events mutex poisoned");
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("tool_use_id=missing"));
        assert!(events[0].contains("content_preview=need the missing value"));
        assert!(events[0].contains("Anthropic will likely reject"));
    }

    #[test]
    fn build_request_parts_emits_one_tool_result_per_id() {
        let messages = vec![
            assistant("", &["X"]),
            tool("X", "paused"),
            assistant("evidence", &[]),
            user("ship it"),
            tool("X", "real"),
        ];

        let request = build_request_parts(&messages, &[], "claude-test", 1024);
        let tool_results: Vec<_> = request
            .messages
            .iter()
            .filter(|message| tool_result_id(message) == Some("X"))
            .collect();

        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_result_content(tool_results[0]), Some("paused"));
        assert_eq!(request.messages.len(), 4);
        assert!(has_tool_use(&request.messages[0], "X"));
        assert_eq!(tool_result_id(&request.messages[1]), Some("X"));
        assert_eq!(request.messages[2].role, "assistant");
        assert!(matches!(
            &request.messages[2].content,
            ApiMessageContent::Text(content) if content == "evidence"
        ));
        assert_eq!(request.messages[3].role, "user");
        assert!(matches!(
            &request.messages[3].content,
            ApiMessageContent::Text(content) if content == "ship it"
        ));
    }
}
