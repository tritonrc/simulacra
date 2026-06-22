//! OpenAI-compatible provider implementation.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use simulacra_types::{
    FinishReason, Message, Provider, ProviderError, ProviderResponse, ResourceBudget, Role,
    TokenUsage, ToolCallMessage, ToolDefinition,
};
use tracing::Instrument;

// ── OTel meters ──────────────────────────────────────────────────

/// Lazily-initialized OTel meter instruments for the OpenAI provider.
/// Created on first use so they pick up the global MeterProvider
/// (which may not be set at construction time).
struct OpenAiMeters {
    duration_histogram: Histogram<f64>,
    token_usage_histogram: Histogram<u64>,
}

impl OpenAiMeters {
    fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<OpenAiMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-provider");
            OpenAiMeters {
                duration_histogram: meter
                    .f64_histogram("gen_ai.client.operation.duration")
                    .with_unit("ms")
                    .with_description("LLM provider call duration")
                    .build(),
                token_usage_histogram: meter
                    .u64_histogram("gen_ai.client.token.usage")
                    .with_unit("{token}")
                    .with_description("Token usage per LLM call")
                    .build(),
            }
        })
    }
}

// ── HTTP abstraction ───────────────────────────────────────────────

/// Minimal HTTP client trait so tests can substitute a fake.
trait HttpClient: Send + Sync {
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + '_>>;
}

/// Raw HTTP response.
struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

// ── Reqwest-backed client ──────────────────────────────────────────

struct ReqwestClient {
    client: reqwest::Client,
}

impl ReqwestClient {
    fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl HttpClient for ReqwestClient {
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + '_>> {
        let url = url.to_owned();
        let headers = headers.to_vec();
        let body = body.to_vec();
        Box::pin(async move {
            let mut builder = self.client.post(&url);
            for (key, value) in &headers {
                builder = builder.header(key.as_str(), value.as_str());
            }
            let resp = builder
                .body(body)
                .send()
                .await
                .map_err(|e| ProviderError::Other(format!("HTTP request failed: {e}")))?;

            let status = resp.status().as_u16();
            let resp_headers: HashMap<String, String> = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|val| (k.as_str().to_lowercase(), val.to_owned()))
                })
                .collect();
            let resp_body = resp
                .bytes()
                .await
                .map_err(|e| ProviderError::Other(format!("failed to read response body: {e}")))?;

            Ok(HttpResponse {
                status,
                headers: resp_headers,
                body: resp_body.to_vec(),
            })
        })
    }
}

// ── OpenAiProvider ───────────────────────────────────────────────

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// OpenAI API provider.
pub struct OpenAiProvider {
    api_key: String,
    model: String,
    base_url: String,
    http: Box<dyn HttpClient>,
}

impl OpenAiProvider {
    /// Create a new OpenAI provider with the given API key and model.
    ///
    /// The base URL is resolved from `OPENAI_BASE_URL` or `OPENAI_API_BASE`
    /// environment variables at construction time, falling back to
    /// `https://api.openai.com`.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let base_url = std::env::var("OPENAI_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_API_BASE"))
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
        Self {
            api_key: api_key.into().trim().to_owned(),
            model: model.into(),
            base_url,
            http: Box::new(ReqwestClient::new()),
        }
    }

    /// Build the request body JSON.
    ///
    /// `max_completion_tokens`: if `Some`, cap the generation length.
    /// Derived from the remaining token budget by the caller.
    fn build_request_body(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
        max_completion_tokens: Option<u64>,
    ) -> serde_json::Value {
        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|msg| {
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
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": api_messages,
            "stream": stream,
        });

        // Request token usage in the final SSE chunk so we get real counts.
        if stream {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        }

        // Cap generation length to remaining budget (0 = unlimited).
        if let Some(cap) = max_completion_tokens {
            body["max_completion_tokens"] = serde_json::json!(cap);
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

        let tool_calls = message_obj
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|tc| {
                        let id = tc.get("id")?.as_str()?.to_string();
                        let func = tc.get("function")?;
                        let name = func.get("name")?.as_str()?.to_string();
                        let args_str = func.get("arguments")?.as_str()?;
                        let arguments: serde_json::Value =
                            serde_json::from_str(args_str).unwrap_or_else(|e| {
                                tracing::warn!(
                                    tool_name = name.as_str(),
                                    raw_args = args_str,
                                    error = %e,
                                    "tool call arguments failed to parse as JSON, falling back to empty object"
                                );
                                serde_json::Value::Object(serde_json::Map::new())
                            });
                        Some(ToolCallMessage {
                            id,
                            name,
                            arguments,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let finish_reason_str = choice
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("stop");

        let finish_reason = match finish_reason_str {
            "stop" => FinishReason::EndTurn,
            "tool_calls" => FinishReason::ToolUse,
            "length" => FinishReason::MaxTokens,
            "content_filter" => FinishReason::StopSequence,
            _ => FinishReason::EndTurn,
        };

        let usage = json.get("usage");
        let input_tokens = usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

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
            provider_response_id: response_id,
            model,
        })
    }

    /// Parse a streaming SSE response body into a single `ProviderResponse`.
    fn parse_sse_response(body: &[u8], model: &str) -> Result<ProviderResponse, ProviderError> {
        let text = std::str::from_utf8(body)
            .map_err(|e| ProviderError::Other(format!("SSE body is not valid UTF-8: {e}")))?;

        let mut response_id: Option<String> = None;
        let mut input_tokens: u64 = 0;
        let mut output_tokens: u64 = 0;
        let mut content = String::new();
        let mut finish_reason: Option<String> = None;
        let mut resp_model: Option<String> = None;

        // Track in-progress tool calls keyed by their `index` field.
        // Each entry holds (id, function_name, arguments_so_far).
        let mut pending_tool_calls: std::collections::BTreeMap<u64, (String, String, String)> =
            std::collections::BTreeMap::new();

        for line in text.lines() {
            let line = line.trim();
            if !line.starts_with("data: ") {
                continue;
            }
            let json_str = &line["data: ".len()..];
            if json_str == "[DONE]" {
                break;
            }
            let event: serde_json::Value = serde_json::from_str(json_str).map_err(|e| {
                ProviderError::Other(format!("failed to parse SSE event JSON: {e}"))
            })?;

            // Extract response ID from first chunk
            if response_id.is_none() {
                response_id = event.get("id").and_then(|v| v.as_str()).map(String::from);
            }
            // Extract model from first chunk
            if resp_model.is_none() {
                resp_model = event
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }

            // Process choices
            if let Some(choices) = event.get("choices").and_then(|c| c.as_array()) {
                for choice in choices {
                    if let Some(delta) = choice.get("delta") {
                        // Accumulate content from delta
                        if let Some(text_content) = delta.get("content").and_then(|v| v.as_str()) {
                            content.push_str(text_content);
                        }

                        // Accumulate tool_calls from delta
                        if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array())
                        {
                            for tc in tool_calls {
                                let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);

                                let entry = pending_tool_calls.entry(idx).or_insert_with(|| {
                                    (String::new(), String::new(), String::new())
                                });

                                // First chunk for this index carries `id` and `function.name`
                                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                    entry.0 = id.to_string();
                                }
                                if let Some(func) = tc.get("function") {
                                    if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                        entry.1 = name.to_string();
                                    }
                                    // Arguments are streamed incrementally — append each chunk
                                    if let Some(args) =
                                        func.get("arguments").and_then(|v| v.as_str())
                                    {
                                        entry.2.push_str(args);
                                    }
                                }
                            }
                        }
                    }

                    // Capture finish_reason
                    if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                        finish_reason = Some(fr.to_string());
                    }
                }
            }

            // Extract usage from the final chunk (choices is empty, usage is present)
            if let Some(usage) = event.get("usage") {
                input_tokens = usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(input_tokens);
                output_tokens = usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(output_tokens);
            }
        }

        // Convert accumulated tool calls into ToolCallMessage values.
        let tool_calls: Vec<ToolCallMessage> = pending_tool_calls
            .into_values()
            .map(|(id, name, args_str)| {
                let arguments: serde_json::Value =
                    serde_json::from_str(&args_str).unwrap_or_else(|e| {
                        tracing::warn!(
                            tool_name = name.as_str(),
                            raw_args = args_str.as_str(),
                            error = %e,
                            "tool call arguments failed to parse as JSON, falling back to empty object"
                        );
                        serde_json::Value::Object(serde_json::Map::new())
                    });
                ToolCallMessage {
                    id,
                    name,
                    arguments,
                }
            })
            .collect();

        let finish = match finish_reason.as_deref() {
            Some("stop") | None => FinishReason::EndTurn,
            Some("tool_calls") => FinishReason::ToolUse,
            Some("length") => FinishReason::MaxTokens,
            _ => FinishReason::EndTurn,
        };

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
            finish_reason: finish,
            provider_response_id: response_id,
            model: resp_model.unwrap_or_else(|| model.to_string()),
        })
    }
}

impl Provider for OpenAiProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        budget: &'a mut ResourceBudget,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>> {
        let model = self.model.clone();
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

            // Calculate remaining token budget for max_completion_tokens.
            // A budget max_tokens of 0 means unlimited — don't cap.
            let max_completion_tokens = if budget.max_tokens == 0 {
                None
            } else {
                let remaining = budget.max_tokens.saturating_sub(budget.used_tokens);
                Some(remaining.max(1))
            };

            let url = format!("{}/v1/chat/completions", self.base_url);

            let request_body =
                self.build_request_body(messages, tools, true, max_completion_tokens);
            let body_bytes = serde_json::to_vec(&request_body)
                .map_err(|e| ProviderError::Other(format!("failed to serialize request: {e}")))?;

            let headers = vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                (
                    "authorization".to_owned(),
                    format!("Bearer {}", self.api_key),
                ),
            ];

            let response = self.http.post(&url, &headers, &body_bytes).await?;

            if response.status != 200 {
                let err = Self::classify_error(response.status, &response.headers, &response.body);
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
                Self::parse_sse_response(&response.body, &self.model)?
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

            Ok(provider_resp)
        };
        Box::pin(fut.instrument(span))
    }
}
