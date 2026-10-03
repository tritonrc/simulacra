//! OpenAI-compatible provider implementation.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use opentelemetry::KeyValue;
use simulacra_types::{
    Message, Provider, ProviderError, ProviderResponse, ProviderStreamEvent, ProviderStreamSink,
    ResourceBudget, Role, StreamingProvider, TokenUsage, ToolCallMessage, ToolDefinition,
};
use tracing::Instrument;

mod config;
mod encode;
mod endpoint;
mod finish;
mod http;
mod meters;

pub use config::{AuthStyle, OpenAiConfig, OutputCapField};

use http::{HttpClient, HttpStreamSink, ReqwestClient};
use meters::OpenAiMeters;

// ── OpenAiProvider ───────────────────────────────────────────────

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// OpenAI API provider.
pub struct OpenAiProvider {
    config: OpenAiConfig,
    http: Box<dyn HttpClient>,
}

impl OpenAiProvider {
    /// Create a new OpenAI provider with the given API key and model.
    ///
    /// The base URL is resolved from `OPENAI_BASE_URL` or `OPENAI_API_BASE`
    /// environment variables at construction time, falling back to
    /// `https://api.openai.com`. For a gateway that needs a different auth
    /// style, extra headers, or output-cap field, build an `OpenAiConfig`
    /// and use `with_config` instead — it reads no environment variables.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let base_url = std::env::var("OPENAI_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_API_BASE"))
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
        Self::with_config(OpenAiConfig::new(base_url, api_key, model))
    }

    /// Create a provider from an explicit configuration. Reads no
    /// environment variables, so it is safe to point at any
    /// OpenAI-compatible gateway (OpenRouter, LiteLLM, vLLM, Azure-style).
    pub fn with_config(config: OpenAiConfig) -> Self {
        Self {
            config,
            http: Box::new(ReqwestClient::new()),
        }
    }

    /// Build the request body JSON.
    ///
    /// `output_cap`: if `Some`, cap the generation length, sent under
    /// whichever field name `config.output_cap_field` selects. Derived from
    /// the remaining token budget by the caller.
    fn build_request_body(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
        output_cap: Option<u64>,
    ) -> serde_json::Value {
        let api_messages: Vec<serde_json::Value> =
            crate::tool_pairs::normalize_tool_pairs(messages)
                .iter()
                .map(encode::message)
                .collect();

        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": api_messages,
            "stream": stream,
        });

        // Request token usage in the final SSE chunk so we get real counts.
        if stream {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        }

        // Cap generation length to remaining budget (0 = unlimited).
        if let Some(cap) = output_cap {
            body[self.config.output_cap_field.wire_name()] = serde_json::json!(cap);
        }

        if !tools.is_empty() {
            let api_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(api_tools);
        }

        body
    }

    /// Classify an HTTP error response, extracting retry-after for 429s.
    fn classify_error(
        config: &OpenAiConfig,
        status: u16,
        headers: &HashMap<String, String>,
        body: &[u8],
    ) -> ProviderError {
        let message = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| String::from_utf8_lossy(body).to_string());
        let message = config.scrub_upstream_text(&message);

        match status {
            429 => {
                let retry_after_ms = headers
                    .get("retry-after")
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|secs| secs * 1000);
                ProviderError::RateLimit { retry_after_ms }
            }
            _ => ProviderError::classify(status, message),
        }
    }

    /// Parse a non-streaming JSON response into a `ProviderResponse`.
    fn parse_json_response(body: &[u8]) -> Result<ProviderResponse, ProviderError> {
        let json: serde_json::Value = serde_json::from_slice(body)
            .map_err(|e| ProviderError::Other(format!("failed to parse response JSON: {e}")))?;

        let response_id = json.get("id").and_then(|v| v.as_str()).map(String::from);

        let model = json
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let choice = json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .ok_or_else(|| ProviderError::Other("no choices in response".to_owned()))?;

        let message_obj = choice
            .get("message")
            .ok_or_else(|| ProviderError::Other("no message in choice".to_owned()))?;

        let content = message_obj
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let tool_calls = Self::parse_tool_calls(message_obj)?;

        let finish_reason_str = choice.get("finish_reason").and_then(|v| v.as_str());
        let refusal_seen = message_obj
            .get("refusal")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());
        let finish_reason = finish::map_finish_reason(finish_reason_str, refusal_seen);

        let usage = json.get("usage");
        let input_tokens = usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read_input_tokens = usage
            .and_then(|u| u.get("prompt_tokens_details"))
            .and_then(|details| details.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(input_tokens);

        Ok(ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content,
                tool_calls,
                tool_call_id: None,
                // OpenAI responses do not include provider-native continuation
                // blocks; Anthropic-only sidecars are intentionally not minted here.
                provider_content: vec![],
            },
            token_usage: TokenUsage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_write_input_tokens: 0,
            },
            finish_reason,
            provider_response_id: response_id,
            model,
        })
    }

    /// Parse a streaming SSE response body into a single `ProviderResponse`.
    fn parse_sse_response(body: &[u8], model: &str) -> Result<ProviderResponse, ProviderError> {
        let text = std::str::from_utf8(body)
            .map_err(|e| ProviderError::Other(format!("SSE body is not valid UTF-8: {e}")))?;

        let mut accumulator = OpenAiSseAccumulator::new(model, None);
        for line in text.lines() {
            accumulator.process_line(line.trim())?;
        }
        accumulator.finish()
    }

    /// Walk a `message.tool_calls` array into `ToolCallMessage`s. A tool
    /// call missing its id/name/arguments shape is skipped as before; one
    /// whose arguments fail to parse as JSON fails the whole response.
    fn parse_tool_calls(
        message_obj: &serde_json::Value,
    ) -> Result<Vec<ToolCallMessage>, ProviderError> {
        let Some(arr) = message_obj.get("tool_calls").and_then(|v| v.as_array()) else {
            return Ok(Vec::new());
        };
        let mut tool_calls = Vec::with_capacity(arr.len());
        for tc in arr {
            let (Some(id), Some(func)) =
                (tc.get("id").and_then(|v| v.as_str()), tc.get("function"))
            else {
                continue;
            };
            let (Some(name), Some(args_str)) = (
                func.get("name").and_then(|v| v.as_str()),
                func.get("arguments").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let arguments = finish::parse_tool_call_arguments(name, args_str)?;
            tool_calls.push(ToolCallMessage {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments,
            });
        }
        Ok(tool_calls)
    }
}

struct OpenAiSseAccumulator<'a> {
    default_model: String,
    provider_sink: Option<&'a dyn ProviderStreamSink>,
    response_id: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    content: String,
    finish_reason: Option<String>,
    refusal_seen: bool,
    resp_model: Option<String>,
    pending_tool_calls: std::collections::BTreeMap<u64, (String, String, String)>,
    done: bool,
}

impl<'a> OpenAiSseAccumulator<'a> {
    fn new(model: &str, provider_sink: Option<&'a dyn ProviderStreamSink>) -> Self {
        Self {
            default_model: model.to_owned(),
            provider_sink,
            response_id: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            content: String::new(),
            finish_reason: None,
            refusal_seen: false,
            resp_model: None,
            pending_tool_calls: std::collections::BTreeMap::new(),
            done: false,
        }
    }

    fn process_line(&mut self, line: &str) -> Result<(), ProviderError> {
        if self.done || !line.starts_with("data: ") {
            return Ok(());
        }
        let json_str = &line["data: ".len()..];
        if json_str == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let event: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| ProviderError::Other(format!("failed to parse SSE event JSON: {e}")))?;

        if self.response_id.is_none() {
            self.response_id = event.get("id").and_then(|v| v.as_str()).map(String::from);
        }
        if self.resp_model.is_none() {
            self.resp_model = event
                .get("model")
                .and_then(|v| v.as_str())
                .map(String::from);
        }

        if let Some(choices) = event.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    if let Some(text_content) = delta.get("content").and_then(|v| v.as_str()) {
                        self.content.push_str(text_content);
                        if let Some(provider_sink) = self.provider_sink
                            && !text_content.is_empty()
                        {
                            provider_sink.emit(ProviderStreamEvent::TextDelta {
                                text: text_content.to_owned(),
                            });
                        }
                    }

                    if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tool_calls {
                            let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                            let entry = self
                                .pending_tool_calls
                                .entry(idx)
                                .or_insert_with(|| (String::new(), String::new(), String::new()));
                            let mut saw_delta = false;
                            let mut arguments_delta = String::new();

                            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                entry.0 = id.to_string();
                                saw_delta = true;
                            }
                            if let Some(func) = tc.get("function") {
                                if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                    entry.1 = name.to_string();
                                    saw_delta = true;
                                }
                                if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                                    entry.2.push_str(args);
                                    arguments_delta = args.to_string();
                                    saw_delta = true;
                                }
                            }
                            if saw_delta && let Some(provider_sink) = self.provider_sink {
                                provider_sink.emit(ProviderStreamEvent::ToolCallDelta {
                                    index: idx,
                                    tool_call_id: (!entry.0.is_empty()).then(|| entry.0.clone()),
                                    name: (!entry.1.is_empty()).then(|| entry.1.clone()),
                                    arguments_delta,
                                });
                            }
                        }
                    }

                    if delta
                        .get("refusal")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.is_empty())
                    {
                        self.refusal_seen = true;
                    }
                }

                if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                    self.finish_reason = Some(fr.to_string());
                }
            }
        }

        if let Some(usage) = event.get("usage") {
            self.input_tokens = usage
                .get("prompt_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(self.input_tokens);
            self.output_tokens = usage
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(self.output_tokens);
            self.cache_read_input_tokens = usage
                .get("prompt_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(self.cache_read_input_tokens)
                .min(self.input_tokens);
        }

        Ok(())
    }

    fn finish(self) -> Result<ProviderResponse, ProviderError> {
        let mut tool_calls = Vec::with_capacity(self.pending_tool_calls.len());
        for (id, name, args_str) in self.pending_tool_calls.into_values() {
            let arguments = finish::parse_tool_call_arguments(&name, &args_str)?;
            tool_calls.push(ToolCallMessage {
                id,
                name,
                arguments,
            });
        }

        let finish_reason =
            finish::map_finish_reason(self.finish_reason.as_deref(), self.refusal_seen);

        Ok(ProviderResponse {
            message: Message {
                role: Role::Assistant,
                content: self.content,
                tool_calls,
                tool_call_id: None,
                // OpenAI streams do not carry Anthropic-style thinking blocks.
                provider_content: vec![],
            },
            token_usage: TokenUsage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cache_read_input_tokens: self.cache_read_input_tokens,
                cache_write_input_tokens: 0,
            },
            finish_reason,
            provider_response_id: self.response_id,
            model: self.resp_model.unwrap_or(self.default_model),
        })
    }
}

struct OpenAiStreamEmitter<'a> {
    accumulator: OpenAiSseAccumulator<'a>,
    active: bool,
    pending: Vec<u8>,
}

impl<'a> OpenAiStreamEmitter<'a> {
    fn new(model: &str, provider_sink: &'a dyn ProviderStreamSink) -> Self {
        Self {
            accumulator: OpenAiSseAccumulator::new(model, Some(provider_sink)),
            active: false,
            pending: Vec::new(),
        }
    }

    fn finish(mut self) -> Result<ProviderResponse, ProviderError> {
        if self.active && !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.process_line_bytes(line)?;
        }
        self.accumulator.finish()
    }

    fn push_bytes(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        if !self.active {
            return Ok(());
        }
        self.pending.extend_from_slice(chunk);
        while let Some(pos) = self.pending.iter().position(|b| *b == b'\n') {
            let line = self.pending.drain(..=pos).collect::<Vec<_>>();
            self.process_line_bytes(line)?;
        }
        Ok(())
    }

    fn process_line_bytes(&mut self, mut line: Vec<u8>) -> Result<(), ProviderError> {
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        let line = std::str::from_utf8(&line)
            .map_err(|e| ProviderError::Other(format!("SSE line is not valid UTF-8: {e}")))?;
        self.process_line(line.trim())
    }

    fn process_line(&mut self, line: &str) -> Result<(), ProviderError> {
        self.accumulator.process_line(line)
    }
}

impl HttpStreamSink for OpenAiStreamEmitter<'_> {
    fn begin(
        &mut self,
        status: u16,
        headers: &HashMap<String, String>,
    ) -> Result<(), ProviderError> {
        self.active = status == 200
            && headers
                .get("content-type")
                .is_some_and(|ct| ct.contains("text/event-stream"));
        Ok(())
    }

    fn chunk(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        self.push_bytes(chunk)
    }
}

impl Provider for OpenAiProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        budget: &'a mut ResourceBudget,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>> {
        let model = self.config.model.clone();
        let otel_name = format!("chat {model}");
        let span = tracing::info_span!(
            "chat",
            "otel.name" = otel_name.as_str(),
            "gen_ai.operation.name" = "chat",
            "gen_ai.request.model" = model.as_str(),
            "gen_ai.provider.name" = "openai",
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "gen_ai.response.id" = tracing::field::Empty,
            "gen_ai.response.finish_reasons" = tracing::field::Empty,
        );

        let fut = async move {
            let call_start = std::time::Instant::now();

            // Check budget before making any HTTP call.
            budget.check_budget()?;

            // Calculate remaining token budget for the output cap.
            // A budget max_tokens of 0 means unlimited — don't cap.
            let output_cap = if budget.max_tokens == 0 {
                None
            } else {
                let remaining = budget.max_tokens.saturating_sub(budget.used_tokens);
                Some(remaining.max(1))
            };

            let url = endpoint::build_chat_completions_url(&self.config.base_url)?;

            let request_body = self.build_request_body(messages, tools, true, output_cap);
            let body_bytes = serde_json::to_vec(&request_body)
                .map_err(|e| ProviderError::Other(format!("failed to serialize request: {e}")))?;

            let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
            headers.extend(self.config.auth_headers());

            let response = self.http.post(&url, &headers, &body_bytes).await?;

            if response.status != 200 {
                let err = Self::classify_error(
                    &self.config,
                    response.status,
                    &response.headers,
                    &response.body,
                );
                if err.is_retryable() {
                    tracing::warn!(
                        error_type = "server_error",
                        status = response.status,
                        "provider error: retryable"
                    );
                } else {
                    tracing::error!(
                        error_type = "client_error",
                        status = response.status,
                        "provider error: non-retryable"
                    );
                }
                return Err(err);
            }

            // Check content-type to determine if this is SSE or JSON.
            let content_type = response
                .headers
                .get("content-type")
                .map(|v| v.as_str())
                .unwrap_or("");

            let provider_resp = if content_type.contains("text/event-stream") {
                Self::parse_sse_response(&response.body, &self.config.model)?
            } else {
                Self::parse_json_response(&response.body)?
            };

            // S010: Record response attributes on the span
            let current_span = tracing::Span::current();
            current_span.record(
                "gen_ai.usage.input_tokens",
                provider_resp.token_usage.input_tokens,
            );
            current_span.record(
                "gen_ai.usage.output_tokens",
                provider_resp.token_usage.output_tokens,
            );
            if let Some(ref id) = provider_resp.provider_response_id {
                current_span.record("gen_ai.response.id", id.as_str());
            }

            // S010: Emit token usage histogram event
            let total_tokens = provider_resp.token_usage.total();
            tracing::info!(
                gen_ai.client.token.usage = total_tokens,
                gen_ai.usage.input_tokens = provider_resp.token_usage.input_tokens,
                gen_ai.usage.output_tokens = provider_resp.token_usage.output_tokens,
                gen_ai.operation.name = "chat",
                gen_ai.request.model = model.as_str(),
                operation = "chat",
                model = model.as_str(),
                "token usage"
            );

            // S010: Emit operation duration histogram event
            let duration_secs = call_start.elapsed().as_secs_f64();
            tracing::info!(
                gen_ai.client.operation.duration = duration_secs,
                gen_ai.operation.name = "chat",
                gen_ai.request.model = model.as_str(),
                operation = "chat",
                model = model.as_str(),
                "operation duration"
            );

            // S010: Emit tool call counter events
            for tc in &provider_resp.message.tool_calls {
                tracing::info!(
                    simulacra.tool.calls = 1u64,
                    tool_name = tc.name.as_str(),
                    source = "builtin",
                    "tool call"
                );
            }

            // S010: Record OTel meter observations
            let meters = OpenAiMeters::get();
            let attrs = &[
                KeyValue::new("gen_ai.operation.name", "chat"),
                KeyValue::new("gen_ai.request.model", model.clone()),
                KeyValue::new("gen_ai.provider.name", "openai"),
            ];
            meters
                .duration_histogram
                .record(call_start.elapsed().as_secs_f64() * 1000.0, attrs);
            meters.token_usage_histogram.record(
                provider_resp.token_usage.input_tokens,
                &[
                    KeyValue::new("gen_ai.operation.name", "chat"),
                    KeyValue::new("gen_ai.request.model", model.clone()),
                    KeyValue::new("gen_ai.provider.name", "openai"),
                    KeyValue::new("gen_ai.token.type", "input"),
                ],
            );
            meters.token_usage_histogram.record(
                provider_resp.token_usage.output_tokens,
                &[
                    KeyValue::new("gen_ai.operation.name", "chat"),
                    KeyValue::new("gen_ai.request.model", model.clone()),
                    KeyValue::new("gen_ai.provider.name", "openai"),
                    KeyValue::new("gen_ai.token.type", "output"),
                ],
            );
            meters.record_cache_usage(&provider_resp.token_usage, &model);

            Ok(provider_resp)
        };
        Box::pin(fut.instrument(span))
    }

    fn as_streaming(&self) -> Option<&dyn StreamingProvider> {
        Some(self)
    }
}

impl StreamingProvider for OpenAiProvider {
    fn chat_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        budget: &'a mut ResourceBudget,
        stream_sink: &'a dyn ProviderStreamSink,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>> {
        let model = self.config.model.clone();
        let otel_name = format!("chat {model}");
        let span = tracing::info_span!(
            "chat",
            "otel.name" = otel_name.as_str(),
            "gen_ai.operation.name" = "chat",
            "gen_ai.request.model" = model.as_str(),
            "gen_ai.provider.name" = "openai",
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "gen_ai.response.id" = tracing::field::Empty,
            "gen_ai.response.finish_reasons" = tracing::field::Empty,
        );

        let fut = async move {
            let call_start = std::time::Instant::now();
            budget.check_budget()?;

            let output_cap = if budget.max_tokens == 0 {
                None
            } else {
                let remaining = budget.max_tokens.saturating_sub(budget.used_tokens);
                Some(remaining.max(1))
            };

            let url = endpoint::build_chat_completions_url(&self.config.base_url)?;
            let request_body = self.build_request_body(messages, tools, true, output_cap);
            let body_bytes = serde_json::to_vec(&request_body)
                .map_err(|e| ProviderError::Other(format!("failed to serialize request: {e}")))?;
            let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
            headers.extend(self.config.auth_headers());

            let mut emitter = OpenAiStreamEmitter::new(&self.config.model, stream_sink);
            let response = self
                .http
                .post_stream(&url, &headers, &body_bytes, &mut emitter)
                .await?;

            if response.status != 200 {
                let err = Self::classify_error(
                    &self.config,
                    response.status,
                    &response.headers,
                    &response.body,
                );
                if err.is_retryable() {
                    tracing::warn!(
                        error_type = "server_error",
                        status = response.status,
                        "provider error: retryable"
                    );
                } else {
                    tracing::error!(
                        error_type = "client_error",
                        status = response.status,
                        "provider error: non-retryable"
                    );
                }
                return Err(err);
            }

            let content_type = response
                .headers
                .get("content-type")
                .map(|v| v.as_str())
                .unwrap_or("");
            let provider_resp = if content_type.contains("text/event-stream") {
                emitter.finish()?
            } else {
                Self::parse_json_response(&response.body)?
            };

            let current_span = tracing::Span::current();
            current_span.record(
                "gen_ai.usage.input_tokens",
                provider_resp.token_usage.input_tokens,
            );
            current_span.record(
                "gen_ai.usage.output_tokens",
                provider_resp.token_usage.output_tokens,
            );
            if let Some(ref id) = provider_resp.provider_response_id {
                current_span.record("gen_ai.response.id", id.as_str());
            }

            let total_tokens = provider_resp.token_usage.total();
            tracing::info!(
                gen_ai.client.token.usage = total_tokens,
                gen_ai.usage.input_tokens = provider_resp.token_usage.input_tokens,
                gen_ai.usage.output_tokens = provider_resp.token_usage.output_tokens,
                gen_ai.operation.name = "chat",
                gen_ai.request.model = model.as_str(),
                operation = "chat",
                model = model.as_str(),
                "token usage"
            );
            let duration_secs = call_start.elapsed().as_secs_f64();
            tracing::info!(
                gen_ai.client.operation.duration = duration_secs,
                gen_ai.operation.name = "chat",
                gen_ai.request.model = model.as_str(),
                operation = "chat",
                model = model.as_str(),
                "operation duration"
            );
            for tc in &provider_resp.message.tool_calls {
                tracing::info!(
                    simulacra.tool.calls = 1u64,
                    tool_name = tc.name.as_str(),
                    source = "builtin",
                    "tool call"
                );
            }

            let meters = OpenAiMeters::get();
            let attrs = &[
                KeyValue::new("gen_ai.operation.name", "chat"),
                KeyValue::new("gen_ai.request.model", model.clone()),
                KeyValue::new("gen_ai.provider.name", "openai"),
            ];
            meters
                .duration_histogram
                .record(call_start.elapsed().as_secs_f64() * 1000.0, attrs);
            meters.token_usage_histogram.record(
                provider_resp.token_usage.input_tokens,
                &[
                    KeyValue::new("gen_ai.operation.name", "chat"),
                    KeyValue::new("gen_ai.request.model", model.clone()),
                    KeyValue::new("gen_ai.provider.name", "openai"),
                    KeyValue::new("gen_ai.token.type", "input"),
                ],
            );
            meters.token_usage_histogram.record(
                provider_resp.token_usage.output_tokens,
                &[
                    KeyValue::new("gen_ai.operation.name", "chat"),
                    KeyValue::new("gen_ai.request.model", model.clone()),
                    KeyValue::new("gen_ai.provider.name", "openai"),
                    KeyValue::new("gen_ai.token.type", "output"),
                ],
            );
            meters.record_cache_usage(&provider_resp.token_usage, &model);

            Ok(provider_resp)
        };
        Box::pin(fut.instrument(span))
    }
}

#[cfg(test)]
mod tests {
    use super::http::HttpResponse;
    use super::*;
    use rust_decimal::Decimal;
    use simulacra_types::FinishReason;
    use std::io::Read;
    use std::net::{Shutdown, SocketAddr, TcpListener};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread::JoinHandle;
    use std::time::Duration;

    struct FakeHttpClient {
        response: Arc<dyn Fn() -> Result<HttpResponse, ProviderError> + Send + Sync>,
    }

    impl FakeHttpClient {
        fn with_response_and_headers(
            status: u16,
            body: &[u8],
            headers: HashMap<String, String>,
        ) -> Self {
            let body = body.to_vec();
            Self {
                response: Arc::new(move || {
                    Ok(HttpResponse {
                        status,
                        headers: headers.clone(),
                        body: body.clone(),
                    })
                }),
            }
        }
    }

    impl HttpClient for FakeHttpClient {
        fn post(
            &self,
            _url: &str,
            _headers: &[(String, String)],
            _body: &[u8],
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + '_>>
        {
            let resp_fn = Arc::clone(&self.response);
            Box::pin(async move { resp_fn() })
        }

        fn post_stream<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(String, String)],
            _body: &'a [u8],
            sink: &'a mut dyn HttpStreamSink,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + 'a>>
        {
            let resp_fn = Arc::clone(&self.response);
            Box::pin(async move {
                let response = resp_fn()?;
                sink.begin(response.status, &response.headers)?;
                sink.chunk(&response.body)?;
                Ok(HttpResponse {
                    status: response.status,
                    headers: response.headers,
                    body: Vec::new(),
                })
            })
        }
    }

    #[derive(Default)]
    struct RecordingProviderStreamSink {
        events: Mutex<Vec<simulacra_types::ProviderStreamEvent>>,
    }

    impl RecordingProviderStreamSink {
        fn texts(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .filter_map(|event| match event {
                    simulacra_types::ProviderStreamEvent::TextDelta { text } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        }

        fn events(&self) -> Vec<simulacra_types::ProviderStreamEvent> {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl simulacra_types::ProviderStreamSink for RecordingProviderStreamSink {
        fn emit(&self, event: simulacra_types::ProviderStreamEvent) {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
        }
    }

    fn assert_transport_retryable(err: ProviderError) {
        assert!(
            matches!(&err, ProviderError::Transport(_)),
            "transport failures must be classified as ProviderError::Transport, got: {err:?}"
        );
        assert!(
            err.is_retryable(),
            "ProviderError::Transport must be retryable, got: {err:?}"
        );
    }

    struct LocalHttpServer {
        addr: SocketAddr,
        release: Option<mpsc::Sender<()>>,
        handle: Option<JoinHandle<()>>,
    }

    impl LocalHttpServer {
        fn abort_connection() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("localhost fixture should bind");
            let addr = listener
                .local_addr()
                .expect("localhost fixture should expose an address");
            let handle = std::thread::spawn(move || {
                let (stream, _) = listener
                    .accept()
                    .expect("fixture should accept one request");
                let _ = stream.shutdown(Shutdown::Both);
            });
            Self {
                addr,
                release: None,
                handle: Some(handle),
            }
        }

        fn stall_before_response() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("localhost fixture should bind");
            let addr = listener
                .local_addr()
                .expect("localhost fixture should expose an address");
            let (release, released) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = listener
                    .accept()
                    .expect("fixture should accept one request");
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = released.recv();
                let _ = stream.shutdown(Shutdown::Both);
            });
            Self {
                addr,
                release: Some(release),
                handle: Some(handle),
            }
        }

        fn url(&self) -> String {
            format!("http://{}/v1/chat/completions", self.addr)
        }
    }

    impl Drop for LocalHttpServer {
        fn drop(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
            let _ = std::net::TcpStream::connect(self.addr);
            if let Some(handle) = self.handle.take() {
                handle.join().expect("fixture should join cleanly");
            }
        }
    }

    struct NoopHttpStreamSink;

    impl HttpStreamSink for NoopHttpStreamSink {}

    fn timeout_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .expect("timeout client should build")
    }

    async fn assert_stalling_fixture_produces_reqwest_timeout() {
        let server = LocalHttpServer::stall_before_response();
        let err = timeout_client()
            .post(server.url())
            .body("{}")
            .send()
            .await
            .expect_err("stalling localhost response should time out");
        assert!(
            err.is_timeout(),
            "fixture must produce a reqwest timeout, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn reqwest_connection_failure_is_transport_and_retryable() {
        let server = LocalHttpServer::abort_connection();
        let client = ReqwestClient::new();
        let err = match client.post(&server.url(), &[], b"{}").await {
            Ok(_) => panic!("aborted localhost connection should fail before a response"),
            Err(err) => err,
        };

        assert_transport_retryable(err);
    }

    #[tokio::test]
    async fn reqwest_request_timeout_is_transport_and_retryable() {
        assert_stalling_fixture_produces_reqwest_timeout().await;
        let server = LocalHttpServer::stall_before_response();
        let client = ReqwestClient {
            client: timeout_client(),
        };
        let err = match client.post(&server.url(), &[], b"{}").await {
            Ok(_) => panic!("request timeout should return a provider error"),
            Err(err) => err,
        };

        assert_transport_retryable(err);
    }

    #[tokio::test]
    async fn reqwest_streaming_connection_failure_is_transport_and_retryable() {
        let server = LocalHttpServer::abort_connection();
        let client = ReqwestClient::new();
        let mut sink = NoopHttpStreamSink;
        let err = match client
            .post_stream(&server.url(), &[], b"{}", &mut sink)
            .await
        {
            Ok(_) => panic!("aborted localhost connection should fail before a response"),
            Err(err) => err,
        };

        assert_transport_retryable(err);
    }

    #[tokio::test]
    async fn reqwest_streaming_request_timeout_is_transport_and_retryable() {
        assert_stalling_fixture_produces_reqwest_timeout().await;
        let server = LocalHttpServer::stall_before_response();
        let client = ReqwestClient {
            client: timeout_client(),
        };
        let mut sink = NoopHttpStreamSink;
        let err = match client
            .post_stream(&server.url(), &[], b"{}", &mut sink)
            .await
        {
            Ok(_) => panic!("streaming request timeout should return a provider error"),
            Err(err) => err,
        };

        assert_transport_retryable(err);
    }

    fn streaming_response_body() -> Vec<u8> {
        concat!(
            "data: {\"id\":\"chatcmpl-stream\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-stream\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-stream\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-stream\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"chatcmpl-stream\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        )
        .as_bytes()
        .to_vec()
    }

    #[tokio::test]
    async fn streaming_provider_emits_openai_text_deltas_and_assembles_final_response() {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_owned(), "text/event-stream".to_owned());
        let provider = OpenAiProvider {
            config: OpenAiConfig::new("https://example.invalid", "test-key", "gpt-4o-mini"),
            http: Box::new(FakeHttpClient::with_response_and_headers(
                200,
                &streaming_response_body(),
                headers,
            )),
        };
        let messages = vec![Message {
            role: Role::User,
            content: "Hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }];
        let mut budget = ResourceBudget::new(100_000, 100, Decimal::new(100, 0), 10);
        let sink = RecordingProviderStreamSink::default();

        let response = simulacra_types::StreamingProvider::chat_stream(
            &provider,
            &messages,
            &[],
            &mut budget,
            &sink,
        )
        .await
        .expect("OpenAI streaming should assemble a final response");

        assert_eq!(sink.texts(), vec!["Hel", "lo"]);
        assert_eq!(response.message.content, "Hello");
        assert_eq!(
            response.provider_response_id.as_deref(),
            Some("chatcmpl-stream")
        );
        assert_eq!(response.token_usage.input_tokens, 3);
        assert_eq!(response.token_usage.output_tokens, 2);
        assert_eq!(response.finish_reason, FinishReason::EndTurn);
    }

    fn streaming_tool_call_response_body() -> Vec<u8> {
        concat!(
            "data: {\"id\":\"chatcmpl-stream-tool\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_tc1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-stream-tool\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"loc\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-stream-tool\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ation\\\":\\\"SF\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-stream-tool\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"id\":\"chatcmpl-stream-tool\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o-mini\",\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":15,\"total_tokens\":35}}\n\n",
            "data: [DONE]\n\n",
        )
        .as_bytes()
        .to_vec()
    }

    #[tokio::test]
    async fn streaming_provider_emits_openai_tool_call_deltas_and_assembles_final_response() {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_owned(), "text/event-stream".to_owned());
        let provider = OpenAiProvider {
            config: OpenAiConfig::new("https://example.invalid", "test-key", "gpt-4o-mini"),
            http: Box::new(FakeHttpClient::with_response_and_headers(
                200,
                &streaming_tool_call_response_body(),
                headers,
            )),
        };
        let messages = vec![Message {
            role: Role::User,
            content: "weather".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }];
        let mut budget = ResourceBudget::new(100_000, 100, Decimal::new(100, 0), 10);
        let sink = RecordingProviderStreamSink::default();

        let response = simulacra_types::StreamingProvider::chat_stream(
            &provider,
            &messages,
            &[],
            &mut budget,
            &sink,
        )
        .await
        .expect("OpenAI streaming should assemble a tool call response");

        let tool_deltas: Vec<_> = sink
            .events()
            .into_iter()
            .filter(|event| {
                matches!(
                    event,
                    simulacra_types::ProviderStreamEvent::ToolCallDelta { .. }
                )
            })
            .collect();
        assert_eq!(
            tool_deltas,
            vec![
                simulacra_types::ProviderStreamEvent::ToolCallDelta {
                    index: 0,
                    tool_call_id: Some("call_tc1".into()),
                    name: Some("get_weather".into()),
                    arguments_delta: String::new(),
                },
                simulacra_types::ProviderStreamEvent::ToolCallDelta {
                    index: 0,
                    tool_call_id: Some("call_tc1".into()),
                    name: Some("get_weather".into()),
                    arguments_delta: "{\"loc".into(),
                },
                simulacra_types::ProviderStreamEvent::ToolCallDelta {
                    index: 0,
                    tool_call_id: Some("call_tc1".into()),
                    name: Some("get_weather".into()),
                    arguments_delta: "ation\":\"SF\"}".into(),
                },
            ]
        );
        assert_eq!(response.message.tool_calls.len(), 1);
        assert_eq!(response.message.tool_calls[0].id, "call_tc1");
        assert_eq!(response.message.tool_calls[0].name, "get_weather");
        assert_eq!(
            response.message.tool_calls[0].arguments,
            serde_json::json!({"location": "SF"})
        );
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
    }
}
