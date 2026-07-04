//! Converse API request/response and converse-stream event mapping.
//!
//! All (de)serialization is done through `serde_json::Value`, mirroring the
//! OpenAI provider: Bedrock's content-block shapes (single-key objects like
//! `{text: ...}`, `{toolUse: {...}}`, `{toolResult: {...}}`) are awkward to
//! express as serde enums, and value-based mapping keeps the surface small.

use simulacra_types::{
    FinishReason, Message, ProviderResponse, ProviderStreamEvent, ProviderStreamSink, Role,
    TokenUsage, ToolCallMessage, ToolDefinition,
};

use crate::bedrock::eventstream::ParsedFrame;

// ── Request building ───────────────────────────────────────────────

/// Build a Converse API request body.
///
/// `max_tokens` is derived from the remaining token budget by the caller.
pub(crate) fn build_request_body(
    messages: &[Message],
    tools: &[ToolDefinition],
    model: &str,
    max_tokens: u32,
) -> serde_json::Value {
    use serde_json::json;

    let mut system_parts: Vec<String> = Vec::new();
    let mut converse_messages: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                if !msg.content.is_empty() {
                    system_parts.push(msg.content.clone());
                }
            }
            Role::User => {
                converse_messages.push(json!({
                    "role": "user",
                    "content": [{ "text": msg.content }]
                }));
            }
            Role::Assistant => {
                let mut blocks: Vec<serde_json::Value> = Vec::new();
                if !msg.content.is_empty() {
                    blocks.push(json!({ "text": msg.content }));
                }
                for tc in &msg.tool_calls {
                    blocks.push(json!({
                        "toolUse": {
                            "toolUseId": tc.id,
                            "name": tc.name,
                            "input": tc.arguments,
                        }
                    }));
                }
                // Converse requires every assistant message to carry content.
                if blocks.is_empty() {
                    blocks.push(json!({ "text": "" }));
                }
                converse_messages.push(json!({
                    "role": "assistant",
                    "content": blocks,
                }));
            }
            Role::Tool => {
                let Some(tool_use_id) = &msg.tool_call_id else {
                    continue;
                };
                // Tool results travel as a user-role message with a toolResult block.
                converse_messages.push(json!({
                    "role": "user",
                    "content": [{
                        "toolResult": {
                            "toolUseId": tool_use_id,
                            "content": [{ "text": msg.content }],
                        }
                    }]
                }));
            }
        }
    }

    let mut body = json!({
        "modelId": model,
        "messages": converse_messages,
        "inferenceConfig": { "maxTokens": max_tokens },
    });

    if !system_parts.is_empty() {
        body["system"] = serde_json::Value::Array(
            system_parts
                .into_iter()
                .map(|t| json!({ "text": t }))
                .collect(),
        );
    }

    if !tools.is_empty() {
        let tool_specs: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "toolSpec": {
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": { "json": t.input_schema },
                    }
                })
            })
            .collect();
        body["toolConfig"] = json!({ "tools": tool_specs });
    }

    body
}

// ── JSON response parsing ──────────────────────────────────────────

/// Parse a (non-streaming) Converse response body into a `ProviderResponse`.
///
/// `provider_response_id` is supplied by the caller (resolved from the
/// `x-amz-request-id` HTTP response header), since the id is not part of the
/// JSON body.
pub(crate) fn parse_json_response(
    body: &[u8],
    default_model: &str,
    provider_response_id: Option<String>,
) -> Result<ProviderResponse, simulacra_types::ProviderError> {
    let json: serde_json::Value = serde_json::from_slice(body).map_err(|e| {
        simulacra_types::ProviderError::Other(format!("failed to parse Converse response: {e}"))
    })?;

    let message = json
        .get("output")
        .and_then(|o| o.get("message"))
        .ok_or_else(|| {
            simulacra_types::ProviderError::Other(
                "Converse response missing output.message".to_owned(),
            )
        })?;

    let (content, tool_calls) = parse_content_blocks(message.get("content"));

    let finish_reason = parse_stop_reason(json.get("stopReason").and_then(|v| v.as_str()));

    let usage = json.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("inputTokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("outputTokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let model = json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(default_model)
        .to_string();

    Ok(ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens,
            output_tokens,
        },
        finish_reason,
        provider_response_id,
        model,
    })
}

/// Turn a Converse `content` array into (joined text, tool_calls).
fn parse_content_blocks(content: Option<&serde_json::Value>) -> (String, Vec<ToolCallMessage>) {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    let Some(blocks) = content.and_then(|c| c.as_array()) else {
        return (String::new(), tool_calls);
    };

    for block in blocks {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            text_parts.push(text.to_string());
        } else if let Some(tool_use) = block.get("toolUse") {
            let id = tool_use
                .get("toolUseId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = tool_use
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input = tool_use
                .get("input")
                .cloned()
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
            tool_calls.push(ToolCallMessage {
                id,
                name,
                arguments: input,
            });
        }
    }

    (text_parts.join(""), tool_calls)
}

fn parse_stop_reason(stop_reason: Option<&str>) -> FinishReason {
    match stop_reason {
        Some("tool_use") => FinishReason::ToolUse,
        Some("max_tokens") => FinishReason::MaxTokens,
        Some("stop_sequence") => FinishReason::StopSequence,
        _ => FinishReason::EndTurn,
    }
}

// ── converse-stream accumulator ────────────────────────────────────

/// Incrementally assembles converse-stream events into a final
/// `ProviderResponse`, optionally forwarding deltas to a sink.
pub(crate) struct ConverseStreamAccumulator<'a> {
    default_model: String,
    provider_sink: Option<&'a dyn ProviderStreamSink>,
    response_id: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    content: String,
    stop_reason: Option<String>,
    /// contentBlockIndex → (toolUseId, name, accumulated input string).
    pending_tool_blocks: std::collections::BTreeMap<u64, (String, String, String)>,
    tool_calls: Vec<ToolCallMessage>,
}

impl<'a> ConverseStreamAccumulator<'a> {
    pub(crate) fn new(
        default_model: &str,
        provider_sink: Option<&'a dyn ProviderStreamSink>,
        response_id: Option<String>,
    ) -> Self {
        Self {
            default_model: default_model.to_owned(),
            provider_sink,
            response_id,
            input_tokens: 0,
            output_tokens: 0,
            content: String::new(),
            stop_reason: None,
            pending_tool_blocks: std::collections::BTreeMap::new(),
            tool_calls: Vec::new(),
        }
    }

    pub(crate) fn set_response_id(&mut self, id: Option<String>) {
        if self.response_id.is_none() {
            self.response_id = id;
        }
    }

    /// Process one decoded converse-stream frame.
    pub(crate) fn process_frame(
        &mut self,
        frame: &ParsedFrame,
    ) -> Result<(), simulacra_types::ProviderError> {
        if frame.message_type.as_deref() == Some("error") {
            let message = frame
                .payload
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("bedrock stream error")
                .to_string();
            return Err(simulacra_types::ProviderError::Other(format!(
                "bedrock stream error: {message}"
            )));
        }

        match frame.event_type.as_deref() {
            Some("messageStart") => { /* role only; id comes from HTTP header */ }
            Some("contentBlockStart") => {
                let index = frame
                    .payload
                    .get("contentBlockIndex")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if let Some(start) = frame.payload.get("start")
                    && let Some(tool_use) = start.get("toolUse")
                {
                    let id = tool_use
                        .get("toolUseId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = tool_use
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.pending_tool_blocks
                        .insert(index, (id.clone(), name.clone(), String::new()));
                    if let Some(sink) = self.provider_sink {
                        sink.emit(ProviderStreamEvent::ToolCallDelta {
                            index,
                            tool_call_id: (!id.is_empty()).then_some(id),
                            name: (!name.is_empty()).then_some(name),
                            arguments_delta: String::new(),
                        });
                    }
                }
            }
            Some("contentBlockDelta") => {
                let index = frame
                    .payload
                    .get("contentBlockIndex")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if let Some(delta) = frame.payload.get("delta") {
                    if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                        self.content.push_str(text);
                        if let Some(sink) = self.provider_sink
                            && !text.is_empty()
                        {
                            sink.emit(ProviderStreamEvent::TextDelta {
                                text: text.to_owned(),
                            });
                        }
                    } else if let Some(tool_use) = delta.get("toolUse") {
                        // `input` is a partial-JSON string fragment.
                        if let Some(partial) = tool_use.get("input").and_then(|v| v.as_str())
                            && let Some(entry) = self.pending_tool_blocks.get_mut(&index)
                        {
                            entry.2.push_str(partial);
                            let arguments_delta = partial.to_string();
                            if let Some(sink) = self.provider_sink {
                                sink.emit(ProviderStreamEvent::ToolCallDelta {
                                    index,
                                    tool_call_id: (!entry.0.is_empty()).then_some(entry.0.clone()),
                                    name: (!entry.1.is_empty()).then_some(entry.1.clone()),
                                    arguments_delta,
                                });
                            }
                        }
                    }
                }
            }
            Some("contentBlockStop") => {
                let index = frame
                    .payload
                    .get("contentBlockIndex")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if let Some((id, name, args_str)) = self.pending_tool_blocks.remove(&index) {
                    let arguments: serde_json::Value = if args_str.trim().is_empty() {
                        serde_json::Value::Object(serde_json::Map::new())
                    } else {
                        serde_json::from_str(&args_str).unwrap_or_else(|e| {
                            tracing::warn!(
                                tool_name = name.as_str(),
                                raw_args = args_str.as_str(),
                                error = %e,
                                "converse-stream toolUse input failed to parse, falling back to empty object"
                            );
                            serde_json::Value::Object(serde_json::Map::new())
                        })
                    };
                    self.tool_calls.push(ToolCallMessage {
                        id,
                        name,
                        arguments,
                    });
                }
            }
            Some("messageDelta") => {
                if let Some(delta) = frame.payload.get("delta") {
                    self.stop_reason = delta
                        .get("stopReason")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                }
            }
            Some("metadata") => {
                if let Some(usage) = frame.payload.get("usage") {
                    self.input_tokens = usage
                        .get("inputTokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(self.input_tokens);
                    self.output_tokens = usage
                        .get("outputTokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(self.output_tokens);
                }
            }
            Some("messageStop") | None | Some(_) => { /* terminal / ignored */ }
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> ProviderResponse {
        let finish_reason = parse_stop_reason(self.stop_reason.as_deref());
        ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: self.content,
                tool_calls: self.tool_calls,
                tool_call_id: None,
            },
            token_usage: TokenUsage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
            },
            finish_reason,
            provider_response_id: self.response_id,
            model: self.default_model,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }

    fn assistant(content: &str, tool_calls: &[(&str, &str, serde_json::Value)]) -> Message {
        Message {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: tool_calls
                .iter()
                .map(|(id, name, args)| ToolCallMessage {
                    id: (*id).into(),
                    name: (*name).into(),
                    arguments: args.clone(),
                })
                .collect(),
            tool_call_id: None,
        }
    }

    fn system(content: &str) -> Message {
        Message {
            role: Role::System,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }

    fn tool_result(id: &str, content: &str) -> Message {
        Message {
            role: Role::Tool,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: Some(id.into()),
        }
    }

    #[test]
    fn builds_text_only_request() {
        let body = build_request_body(&[user("hi")], &[], "anthropic.claude-3-5-sonnet-v1:0", 512);
        assert_eq!(body["modelId"], "anthropic.claude-3-5-sonnet-v1:0");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(body["inferenceConfig"]["maxTokens"], 512);
        assert!(body.get("system").is_none());
        assert!(body.get("toolConfig").is_none());
    }

    #[test]
    fn lifts_system_messages_out_of_messages() {
        let body = build_request_body(&[system("be brief"), user("hi")], &[], "m", 128);
        assert_eq!(body["system"][0]["text"], "be brief");
        // System must not appear in the messages array.
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn emits_tool_config_when_tools_present() {
        let tool = ToolDefinition {
            name: "get_weather".into(),
            description: "weather".into(),
            input_schema: json!({"type":"object"}),
        };
        let body = build_request_body(&[user("hi")], &[tool], "m", 128);
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["name"],
            "get_weather"
        );
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"],
            "object"
        );
    }

    #[test]
    fn assistant_tool_use_round_trips_into_tooluse_blocks() {
        let msgs = vec![
            assistant(
                "checking",
                &[("tu_1", "get_weather", json!({"location":"SF"}))],
            ),
            tool_result("tu_1", "sunny"),
        ];
        let body = build_request_body(&msgs, &[], "m", 128);
        let assistant_msg = &body["messages"][0];
        assert_eq!(assistant_msg["role"], "assistant");
        assert_eq!(assistant_msg["content"][0]["text"], "checking");
        assert_eq!(assistant_msg["content"][1]["toolUse"]["toolUseId"], "tu_1");
        assert_eq!(
            assistant_msg["content"][1]["toolUse"]["name"],
            "get_weather"
        );
        assert_eq!(
            assistant_msg["content"][1]["toolUse"]["input"]["location"],
            "SF"
        );

        let result_msg = &body["messages"][1];
        assert_eq!(result_msg["role"], "user");
        assert_eq!(result_msg["content"][0]["toolResult"]["toolUseId"], "tu_1");
        assert_eq!(
            result_msg["content"][0]["toolResult"]["content"][0]["text"],
            "sunny"
        );
    }

    #[test]
    fn parses_text_response() {
        let body = serde_json::to_vec(&json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{ "text": "Hello!" }]
                }
            },
            "stopReason": "end_turn",
            "usage": { "inputTokens": 10, "outputTokens": 5 },
            "model": "anthropic.claude-3-5-sonnet-v1:0"
        }))
        .unwrap();

        let resp = parse_json_response(&body, "default", Some("req-1".into())).unwrap();
        assert_eq!(resp.message.content, "Hello!");
        assert!(resp.message.tool_calls.is_empty());
        assert_eq!(resp.token_usage.input_tokens, 10);
        assert_eq!(resp.token_usage.output_tokens, 5);
        assert_eq!(resp.finish_reason, FinishReason::EndTurn);
        assert_eq!(resp.provider_response_id.as_deref(), Some("req-1"));
        assert_eq!(resp.model, "anthropic.claude-3-5-sonnet-v1:0");
    }

    #[test]
    fn parses_tool_use_response() {
        let body = serde_json::to_vec(&json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{
                        "toolUse": {
                            "toolUseId": "tu_9",
                            "name": "get_weather",
                            "input": { "location": "SF" }
                        }
                    }]
                }
            },
            "stopReason": "tool_use",
            "usage": { "inputTokens": 7, "outputTokens": 11 }
        }))
        .unwrap();

        let resp = parse_json_response(&body, "default", None).unwrap();
        assert_eq!(resp.message.tool_calls.len(), 1);
        assert_eq!(resp.message.tool_calls[0].id, "tu_9");
        assert_eq!(resp.message.tool_calls[0].name, "get_weather");
        assert_eq!(
            resp.message.tool_calls[0].arguments,
            json!({"location":"SF"})
        );
        assert_eq!(resp.finish_reason, FinishReason::ToolUse);
    }
}
