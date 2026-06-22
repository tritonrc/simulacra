//! Serde models for the Anthropic Messages API.

use serde::{Deserialize, Serialize};

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
    FinishReason, Message, ProviderResponse, Role, TokenUsage, ToolCallMessage, ToolDefinition,
};

/// Convert simulacra Messages into Anthropic API messages, extracting system messages separately.
/// `max_tokens` should be derived from the remaining budget by the caller.
pub(crate) fn build_request_parts<'a>(
    messages: &'a [Message],
    tools: &'a [ToolDefinition],
    model: &'a str,
    max_tokens: u32,
) -> ApiRequest<'a> {
    let mut system_text: Option<String> = None;
    let mut api_messages: Vec<ApiMessage> = Vec::new();

    for msg in messages {
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
                let mut blocks: Vec<ApiRequestContentBlock> = Vec::new();
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
                let content = if blocks.len() == 1 && msg.tool_calls.is_empty() {
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
                // Missing tool_call_id is a data integrity issue: sending an empty
                // tool_use_id guarantees Anthropic rejects the request with an
                // unmatched tool result error, so we skip the message entirely
                // and log loudly instead of emitting a malformed payload.
                let Some(tool_call_id) = msg.tool_call_id.clone() else {
                    tracing::error!(
                        content_preview = %msg.content.chars().take(120).collect::<String>(),
                        "Role::Tool message missing tool_call_id — skipping (would produce malformed Anthropic request)"
                    );
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
