//! AnthropicProvider implementation.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use simulacra_types::{
    Message, Provider, ProviderError, ProviderResponse, ResourceBudget, ToolDefinition,
};
use tracing::Instrument;

// ── OTel meters ──────────────────────────────────────────────────

/// Lazily-initialized OTel meter instruments for the provider.
/// Created on first use so they pick up the global MeterProvider
/// (which may not be set at construction time).
struct ProviderMeters {
    duration_histogram: Histogram<f64>,
    token_usage_histogram: Histogram<u64>,
}

impl ProviderMeters {
    fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<ProviderMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-provider");
            ProviderMeters {
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

use crate::anthropic::api_types;

// ── HTTP abstraction ───────────────────────────────────────────────

/// Minimal HTTP client trait so tests can substitute a fake.
pub(crate) trait HttpClient: Send + Sync {
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + '_>>;
}

/// Raw HTTP response.
pub(crate) struct HttpResponse {
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
                .map_err(|e| ProviderError::Other(format!("HTTP request failed: {e:?}")))?;

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

// ── AnthropicProvider ──────────────────────────────────────────────

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic API provider.
pub struct AnthropicProvider {
    api_key: String,
    model: String,
    http: Box<dyn HttpClient>,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider with the given API key and model.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into().trim().to_owned(),
            model: model.into(),
            http: Box::new(ReqwestClient::new()),
        }
    }

    /// Create a provider with a custom HTTP client (for testing).
    #[cfg(test)]
    pub(crate) fn with_http_client(
        api_key: impl Into<String>,
        model: impl Into<String>,
        http: Box<dyn HttpClient>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            http,
        }
    }

    /// Returns the configured model name.
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// Parse a streaming SSE response body into a single `ProviderResponse`.
///
/// Anthropic SSE events follow this pattern:
/// - `message_start`: contains the message ID and initial usage (input_tokens)
/// - `content_block_start`: signals a new content block — for `tool_use` blocks,
///   this carries the block's `id` and `name`
/// - `content_block_delta`: incremental updates — `text_delta` for text blocks,
///   `input_json_delta` (with `partial_json`) for tool_use blocks
/// - `content_block_stop`: finalizes a block. For tool_use blocks we parse the
///   accumulated partial_json into a `ToolCallMessage`.
/// - `message_delta`: contains stop_reason and final usage (output_tokens)
/// - `message_stop`: signals the end of the stream
fn parse_sse_to_provider_response(
    body: &[u8],
    model: &str,
) -> Result<ProviderResponse, ProviderError> {
    use simulacra_types::{
        FinishReason, Message, ProviderResponse, Role, TokenUsage, ToolCallMessage,
    };

    let text = std::str::from_utf8(body)
        .map_err(|e| ProviderError::Other(format!("SSE body is not valid UTF-8: {e}")))?;

    let mut response_id: Option<String> = None;
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut content = String::new();
    let mut stop_reason: Option<String> = None;

    // In-flight tool_use content blocks keyed by SSE block index.
    // Holds (id, name, partial_json_accumulator).
    let mut pending_tool_blocks: std::collections::BTreeMap<u64, (String, String, String)> =
        std::collections::BTreeMap::new();
    // Track which block indices are tool_use so content_block_stop can finalize them.
    let mut tool_block_indices: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    // Completed tool calls, in the order their content_block_stop events arrived.
    let mut tool_calls: Vec<ToolCallMessage> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("data: ") {
            continue;
        }
        let json_str = &line["data: ".len()..];
        let event: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| ProviderError::Other(format!("failed to parse SSE event JSON: {e}")))?;

        match event.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                if let Some(msg) = event.get("message") {
                    response_id = msg.get("id").and_then(|v| v.as_str()).map(String::from);
                    if let Some(usage) = msg.get("usage") {
                        input_tokens = usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }
                }
            }
            Some("content_block_start") => {
                // Track tool_use blocks so we can accumulate their JSON input
                // across content_block_delta events and finalize on stop.
                let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(block) = event.get("content_block") {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if block_type == "tool_use" {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        pending_tool_blocks.insert(index, (id, name, String::new()));
                        tool_block_indices.insert(index);
                    }
                }
            }
            Some("content_block_delta") => {
                let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(delta) = event.get("delta") {
                    let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match delta_type {
                        "text_delta" => {
                            if let Some(text_delta) = delta.get("text").and_then(|v| v.as_str()) {
                                content.push_str(text_delta);
                            }
                        }
                        "input_json_delta" => {
                            if let Some(partial) =
                                delta.get("partial_json").and_then(|v| v.as_str())
                                && let Some(entry) = pending_tool_blocks.get_mut(&index)
                            {
                                entry.2.push_str(partial);
                            }
                        }
                        _ => {
                            // Unknown delta type — fall back to legacy shape where
                            // a `text` field may appear at the top of delta.
                            if let Some(text_delta) = delta.get("text").and_then(|v| v.as_str()) {
                                content.push_str(text_delta);
                            }
                        }
                    }
                }
            }
            Some("content_block_stop") => {
                let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                if tool_block_indices.remove(&index)
                    && let Some((id, name, args_str)) = pending_tool_blocks.remove(&index)
                {
                    // Anthropic sends an empty partial_json stream for tools with
                    // no arguments — treat that as an empty object.
                    let arguments: serde_json::Value = if args_str.trim().is_empty() {
                        serde_json::Value::Object(serde_json::Map::new())
                    } else {
                        serde_json::from_str(&args_str).unwrap_or_else(|e| {
                            tracing::warn!(
                                tool_name = name.as_str(),
                                raw_args = args_str.as_str(),
                                error = %e,
                                "tool_use input_json failed to parse, falling back to empty object"
                            );
                            serde_json::Value::Object(serde_json::Map::new())
                        })
                    };
                    tool_calls.push(ToolCallMessage {
                        id,
                        name,
                        arguments,
                    });
                }
            }
            Some("message_delta") => {
                if let Some(delta) = event.get("delta") {
                    stop_reason = delta
                        .get("stop_reason")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                }
                if let Some(usage) = event.get("usage") {
                    output_tokens = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }
            }
            Some("message_stop") => {}
            _ => {}
        }
    }

    let finish_reason = match stop_reason.as_deref() {
        Some("tool_use") => FinishReason::ToolUse,
        Some("max_tokens") => FinishReason::MaxTokens,
        Some("stop_sequence") => FinishReason::StopSequence,
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
        finish_reason,
        provider_response_id: response_id,
        model: model.to_owned(),
    })
}

impl Provider for AnthropicProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        budget: &'a mut ResourceBudget,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>> {
        // S007: Budget is checked BEFORE making the API call.
        // Do synchronous work (budget check, serialization) before entering async.
        let budget_check = budget.check_budget();
        if let Err(e) = budget_check {
            return Box::pin(async move { Err(ProviderError::BudgetExhausted(e)) });
        }

        // Build and serialize the request body synchronously.
        // Derive max_tokens from remaining budget, clamped to a sane range.
        // A budget max_tokens of 0 means unlimited — use the default cap.
        let max_tokens = if budget.max_tokens == 0 {
            8192u32
        } else {
            let remaining = budget.max_tokens.saturating_sub(budget.used_tokens);
            (remaining.min(8192) as u32).max(1)
        };
        let api_req = api_types::build_request_parts(messages, tools, &self.model, max_tokens);
        let body = match serde_json::to_vec(&api_req) {
            Ok(b) => b,
            Err(e) => {
                return Box::pin(async move {
                    Err(ProviderError::Other(format!(
                        "request serialization failed: {e}"
                    )))
                });
            }
        };

        let headers = vec![
            ("x-api-key".to_owned(), self.api_key.clone()),
            ("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
        ];

        let http = &*self.http;
        let model = self.model.clone();

        // S010: Create the OTel GenAI span with pre-call attributes.
        // Response attributes (token counts, response ID, finish reasons) are
        // recorded after the HTTP call completes.
        let otel_name = format!("chat {model}");
        let span = tracing::info_span!(
            "chat",
            "otel.name" = otel_name.as_str(),
            "gen_ai.operation.name" = "chat",
            "gen_ai.request.model" = model.as_str(),
            "gen_ai.provider.name" = "anthropic",
            "gen_ai.request.max_tokens" = max_tokens as u64,
            "server.address" = "api.anthropic.com",
            "server.port" = 443u64,
            // Response attributes — recorded after the call
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "gen_ai.response.id" = tracing::field::Empty,
            "gen_ai.response.finish_reasons" = tracing::field::Empty,
        );

        let fut = async move {
            let call_start = std::time::Instant::now();

            let http_resp = http.post(ANTHROPIC_API_URL, &headers, &body).await?;

            if http_resp.status != 200 {
                // Try to parse the error body for a message.
                let error_message =
                    serde_json::from_slice::<api_types::ApiErrorResponse>(&http_resp.body)
                        .map(|e| e.error.message)
                        .unwrap_or_else(|_| String::from_utf8_lossy(&http_resp.body).into_owned());

                // Special handling for 429: extract retry-after header.
                if http_resp.status == 429 {
                    let retry_after_ms = http_resp
                        .headers
                        .get("retry-after")
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(|secs| secs * 1000);
                    // S010: Log retryable errors at WARN
                    tracing::warn!(
                        error_type = "rate_limit",
                        message = error_message.as_str(),
                        "provider error: rate limited"
                    );
                    return Err(ProviderError::RateLimit { retry_after_ms });
                }

                let err = ProviderError::classify(http_resp.status, error_message.clone());
                if err.is_retryable() {
                    // S010: Log retryable errors at WARN
                    tracing::warn!(
                        error_type = "server_error",
                        message = error_message.as_str(),
                        "provider error: retryable"
                    );
                } else {
                    // S010: Log non-retryable errors at ERROR
                    tracing::error!(
                        error_type = "client_error",
                        message = error_message.as_str(),
                        "provider error: non-retryable"
                    );
                }
                return Err(err);
            }

            // Detect streaming (SSE) vs synchronous JSON response.
            let is_sse = http_resp
                .headers
                .get("content-type")
                .map(|ct| ct.contains("text/event-stream"))
                .unwrap_or(false);

            let provider_resp = if is_sse {
                parse_sse_to_provider_response(&http_resp.body, &model)?
            } else {
                let api_resp: api_types::ApiResponse = serde_json::from_slice(&http_resp.body)
                    .map_err(|e| {
                        ProviderError::Other(format!("failed to parse Anthropic response: {e}"))
                    })?;
                api_types::into_provider_response(api_resp)
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
            let finish_reason_str = serde_json::to_string(&provider_resp.finish_reason)
                .unwrap_or_else(|_| format!("{:?}", provider_resp.finish_reason));
            let finish_reasons = format!("[{finish_reason_str}]");
            current_span.record("gen_ai.response.finish_reasons", finish_reasons.as_str());

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

            // Budget accounting is the caller's responsibility (AgentLoop).
            // Provider returns token usage but does not mutate the budget.

            // S010: Record OTel meter observations
            let meters = ProviderMeters::get();
            let attrs = &[
                KeyValue::new("gen_ai.operation.name", "chat"),
                KeyValue::new("gen_ai.request.model", model.clone()),
                KeyValue::new("gen_ai.provider.name", "anthropic"),
            ];
            meters
                .duration_histogram
                .record(call_start.elapsed().as_secs_f64() * 1000.0, attrs);
            meters.token_usage_histogram.record(
                provider_resp.token_usage.input_tokens,
                &[
                    KeyValue::new("gen_ai.operation.name", "chat"),
                    KeyValue::new("gen_ai.request.model", model.clone()),
                    KeyValue::new("gen_ai.provider.name", "anthropic"),
                    KeyValue::new("gen_ai.token.type", "input"),
                ],
            );
            meters.token_usage_histogram.record(
                provider_resp.token_usage.output_tokens,
                &[
                    KeyValue::new("gen_ai.operation.name", "chat"),
                    KeyValue::new("gen_ai.request.model", model.clone()),
                    KeyValue::new("gen_ai.provider.name", "anthropic"),
                    KeyValue::new("gen_ai.token.type", "output"),
                ],
            );

            Ok(provider_resp)
        };
        Box::pin(fut.instrument(span))
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use simulacra_types::{FinishReason, ToolDefinition};
    use std::sync::Arc;

    /// A fake HTTP client that returns a pre-configured response.
    struct FakeHttpClient {
        response: Arc<dyn Fn() -> Result<HttpResponse, ProviderError> + Send + Sync>,
    }

    impl FakeHttpClient {
        fn with_response(status: u16, body: &[u8]) -> Self {
            let body = body.to_vec();
            Self {
                response: Arc::new(move || {
                    Ok(HttpResponse {
                        status,
                        headers: HashMap::new(),
                        body: body.clone(),
                    })
                }),
            }
        }

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
    }

    /// A fake that captures the request body for inspection.
    struct CapturingHttpClient {
        captured: Arc<tokio::sync::Mutex<Vec<Vec<u8>>>>,
        status: u16,
        response_body: Vec<u8>,
    }

    impl CapturingHttpClient {
        fn new(status: u16, response_body: &[u8]) -> (Self, Arc<tokio::sync::Mutex<Vec<Vec<u8>>>>) {
            let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            (
                Self {
                    captured: Arc::clone(&captured),
                    status,
                    response_body: response_body.to_vec(),
                },
                captured,
            )
        }
    }

    impl HttpClient for CapturingHttpClient {
        fn post(
            &self,
            _url: &str,
            _headers: &[(String, String)],
            body: &[u8],
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + '_>>
        {
            let captured = Arc::clone(&self.captured);
            let body = body.to_vec();
            let status = self.status;
            let response_body = self.response_body.clone();
            Box::pin(async move {
                captured.lock().await.push(body);
                Ok(HttpResponse {
                    status,
                    headers: HashMap::new(),
                    body: response_body,
                })
            })
        }
    }

    fn success_response_json() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": "msg_test123",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Hello, world!"}
            ],
            "model": "claude-sonnet-4-20250514",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 25}
        }))
        .unwrap()
    }

    fn tool_use_response_json() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": "msg_tool456",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Let me check the weather."},
                {
                    "type": "tool_use",
                    "id": "toolu_abc123",
                    "name": "get_weather",
                    "input": {"location": "San Francisco"}
                }
            ],
            "model": "claude-sonnet-4-20250514",
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 50, "output_tokens": 100}
        }))
        .unwrap()
    }

    fn streaming_response_body() -> Vec<u8> {
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_stream789\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-20250514\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":11,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\", stream!\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        )
        .as_bytes()
        .to_vec()
    }

    fn fresh_budget() -> ResourceBudget {
        ResourceBudget::new(100_000, 100, Decimal::new(100, 0), 10)
    }

    fn exhausted_budget() -> ResourceBudget {
        let mut b = ResourceBudget::new(100, 100, Decimal::new(100, 0), 10);
        b.used_tokens = 100; // at limit
        b
    }

    // ── Test 1: Budget check before API call (S007 assertion 1) ────

    #[tokio::test]
    async fn budget_exhausted_returns_error_without_http_call() {
        // The fake will panic if called, proving no HTTP request was made.
        let fake = FakeHttpClient {
            response: Arc::new(|| panic!("HTTP should not be called when budget exhausted")),
        };
        let provider = AnthropicProvider::with_http_client(
            "test-key",
            "claude-sonnet-4-20250514",
            Box::new(fake),
        );

        let messages = vec![Message {
            role: simulacra_types::Role::User,
            content: "Hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }];
        let mut budget = exhausted_budget();

        let result = provider.chat(&messages, &[], &mut budget).await;
        assert!(result.is_err());
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                ProviderError::BudgetExhausted(_)
            ),
            "expected BudgetExhausted, got: {:?}",
            result.unwrap_err()
        );
    }

    // ── Test 2: Successful text response ───────────────────────────

    #[tokio::test]
    async fn successful_text_response_maps_correctly() {
        let fake = FakeHttpClient::with_response(200, &success_response_json());
        let provider = AnthropicProvider::with_http_client(
            "test-key",
            "claude-sonnet-4-20250514",
            Box::new(fake),
        );

        let messages = vec![Message {
            role: simulacra_types::Role::User,
            content: "Hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }];
        let mut budget = fresh_budget();

        let resp = provider.chat(&messages, &[], &mut budget).await.unwrap();

        assert_eq!(resp.message.content, "Hello, world!");
        assert_eq!(resp.message.role, simulacra_types::Role::Assistant);
        assert!(resp.message.tool_calls.is_empty());
        assert_eq!(resp.token_usage.input_tokens, 10);
        assert_eq!(resp.token_usage.output_tokens, 25);
        assert_eq!(resp.finish_reason, FinishReason::EndTurn);
        assert_eq!(resp.provider_response_id, Some("msg_test123".to_string()));
        assert_eq!(resp.model, "claude-sonnet-4-20250514");
    }

    // ── Test 3: Tool use response ──────────────────────────────────

    #[tokio::test]
    async fn tool_use_response_maps_tool_calls() {
        let fake = FakeHttpClient::with_response(200, &tool_use_response_json());
        let provider = AnthropicProvider::with_http_client(
            "test-key",
            "claude-sonnet-4-20250514",
            Box::new(fake),
        );

        let messages = vec![Message {
            role: simulacra_types::Role::User,
            content: "What's the weather?".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }];
        let mut budget = fresh_budget();

        let resp = provider.chat(&messages, &[], &mut budget).await.unwrap();

        assert_eq!(resp.message.content, "Let me check the weather.");
        assert_eq!(resp.message.tool_calls.len(), 1);
        assert_eq!(resp.message.tool_calls[0].id, "toolu_abc123");
        assert_eq!(resp.message.tool_calls[0].name, "get_weather");
        assert_eq!(
            resp.message.tool_calls[0].arguments,
            serde_json::json!({"location": "San Francisco"})
        );
        assert_eq!(resp.finish_reason, FinishReason::ToolUse);
        assert_eq!(resp.token_usage.input_tokens, 50);
        assert_eq!(resp.token_usage.output_tokens, 100);
    }

    // ── Test 4: Error classification ───────────────────────────────

    #[tokio::test]
    async fn rate_limit_429_is_retryable_with_retry_after() {
        let mut headers = HashMap::new();
        headers.insert("retry-after".to_owned(), "30".to_owned());
        let error_body = serde_json::to_vec(&serde_json::json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "too many requests"}
        }))
        .unwrap();
        let fake = FakeHttpClient::with_response_and_headers(429, &error_body, headers);
        let provider = AnthropicProvider::with_http_client(
            "test-key",
            "claude-sonnet-4-20250514",
            Box::new(fake),
        );

        let messages = vec![Message {
            role: simulacra_types::Role::User,
            content: "Hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }];
        let mut budget = fresh_budget();

        let err = provider
            .chat(&messages, &[], &mut budget)
            .await
            .unwrap_err();
        assert!(err.is_retryable());
        match err {
            ProviderError::RateLimit { retry_after_ms } => {
                assert_eq!(retry_after_ms, Some(30_000));
            }
            other => panic!("expected RateLimit, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn auth_error_401_is_not_retryable() {
        let error_body = serde_json::to_vec(&serde_json::json!({
            "type": "error",
            "error": {"type": "authentication_error", "message": "invalid api key"}
        }))
        .unwrap();
        let fake = FakeHttpClient::with_response(401, &error_body);
        let provider = AnthropicProvider::with_http_client(
            "test-key",
            "claude-sonnet-4-20250514",
            Box::new(fake),
        );

        let messages = vec![Message {
            role: simulacra_types::Role::User,
            content: "Hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }];
        let mut budget = fresh_budget();

        let err = provider
            .chat(&messages, &[], &mut budget)
            .await
            .unwrap_err();
        assert!(!err.is_retryable());
        assert!(matches!(err, ProviderError::AuthError(_)));
    }

    #[tokio::test]
    async fn bad_request_400_is_not_retryable() {
        let error_body = serde_json::to_vec(&serde_json::json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": "max_tokens: must be positive"}
        }))
        .unwrap();
        let fake = FakeHttpClient::with_response(400, &error_body);
        let provider = AnthropicProvider::with_http_client(
            "test-key",
            "claude-sonnet-4-20250514",
            Box::new(fake),
        );

        let messages = vec![Message {
            role: simulacra_types::Role::User,
            content: "Hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }];
        let mut budget = fresh_budget();

        let err = provider
            .chat(&messages, &[], &mut budget)
            .await
            .unwrap_err();
        assert!(!err.is_retryable());
        assert!(matches!(err, ProviderError::BadRequest(_)));
    }

    #[tokio::test]
    async fn overloaded_529_is_retryable() {
        let error_body = serde_json::to_vec(&serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "API is overloaded"}
        }))
        .unwrap();
        let fake = FakeHttpClient::with_response(529, &error_body);
        let provider = AnthropicProvider::with_http_client(
            "test-key",
            "claude-sonnet-4-20250514",
            Box::new(fake),
        );

        let messages = vec![Message {
            role: simulacra_types::Role::User,
            content: "Hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }];
        let mut budget = fresh_budget();

        let err = provider
            .chat(&messages, &[], &mut budget)
            .await
            .unwrap_err();
        assert!(err.is_retryable());
        assert!(matches!(err, ProviderError::Overloaded(_)));
    }

    // ── Test 5: Request serialization ──────────────────────────────

    #[tokio::test]
    async fn request_body_has_correct_structure() {
        let (capturing, captured) = CapturingHttpClient::new(200, &success_response_json());
        let provider = AnthropicProvider::with_http_client(
            "test-key",
            "claude-sonnet-4-20250514",
            Box::new(capturing),
        );

        let messages = vec![
            Message {
                role: simulacra_types::Role::System,
                content: "You are helpful.".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ];
        let tools = vec![ToolDefinition {
            name: "get_weather".into(),
            description: "Get weather for a location".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {"type": "string"}
                }
            }),
        }];
        let mut budget = fresh_budget();

        let _ = provider.chat(&messages, &tools, &mut budget).await;

        let bodies = captured.lock().await;
        assert_eq!(bodies.len(), 1);
        let req: serde_json::Value = serde_json::from_slice(&bodies[0]).unwrap();

        assert_eq!(req["model"], "claude-sonnet-4-20250514");
        assert_eq!(req["system"], "You are helpful.");
        // max_tokens derived from budget: min(100000, 8192) = 8192
        assert_eq!(req["max_tokens"], 8192);

        // Messages should NOT contain the system message
        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "Hello");

        // Tools
        let tools_arr = req["tools"].as_array().unwrap();
        assert_eq!(tools_arr.len(), 1);
        assert_eq!(tools_arr[0]["name"], "get_weather");
        assert_eq!(tools_arr[0]["description"], "Get weather for a location");
        assert!(tools_arr[0]["input_schema"].is_object());
    }

    // ── Test 6: Provider returns usage but does NOT mutate budget ──

    #[tokio::test]
    async fn provider_returns_usage_without_mutating_budget() {
        let fake = FakeHttpClient::with_response(200, &success_response_json());
        let provider = AnthropicProvider::with_http_client(
            "test-key",
            "claude-sonnet-4-20250514",
            Box::new(fake),
        );

        let messages = vec![Message {
            role: simulacra_types::Role::User,
            content: "Hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }];
        let mut budget = fresh_budget();
        assert_eq!(budget.used_tokens, 0);

        let resp = provider.chat(&messages, &[], &mut budget).await.unwrap();

        // Provider returns usage in the response...
        assert_eq!(resp.token_usage.total(), 35);
        // ...but does NOT mutate budget (caller owns budget accounting)
        assert_eq!(budget.used_tokens, 0);
    }

    // ── Test: Provider trait is object-safe ─────────────────────────

    #[tokio::test]
    async fn provider_trait_is_object_safe() {
        let fake = FakeHttpClient::with_response(200, &success_response_json());
        let provider: Box<dyn Provider> = Box::new(AnthropicProvider::with_http_client(
            "key",
            "model",
            Box::new(fake),
        ));

        let messages = vec![Message {
            role: simulacra_types::Role::User,
            content: "Hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }];
        let mut budget = fresh_budget();

        // This compiles and works, proving object safety.
        let result = provider.chat(&messages, &[], &mut budget).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn streaming_event_stream_is_assembled_into_final_provider_response() {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_owned(), "text/event-stream".to_owned());
        let fake =
            FakeHttpClient::with_response_and_headers(200, &streaming_response_body(), headers);
        let provider = AnthropicProvider::with_http_client(
            "test-key",
            "claude-sonnet-4-20250514",
            Box::new(fake),
        );

        let messages = vec![Message {
            role: simulacra_types::Role::User,
            content: "Say hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }];
        let mut budget = fresh_budget();

        let resp = provider
            .chat(&messages, &[], &mut budget)
            .await
            .expect("streaming responses should assemble into a final ProviderResponse");

        assert_eq!(resp.message.role, simulacra_types::Role::Assistant);
        assert_eq!(resp.message.content, "Hello, stream!");
        assert!(resp.message.tool_calls.is_empty());
        assert_eq!(resp.token_usage.input_tokens, 11);
        assert_eq!(resp.token_usage.output_tokens, 7);
        assert_eq!(resp.finish_reason, FinishReason::EndTurn);
        assert_eq!(resp.provider_response_id, Some("msg_stream789".to_string()));
        assert_eq!(resp.model, "claude-sonnet-4-20250514");
    }

    /// S007: Multiple provider backends can be selected by configuration.
    ///
    /// Verifies that both AnthropicProvider and OpenAiProvider can be
    /// constructed and used behind `Box<dyn Provider>`, proving the crate
    /// exposes multiple backends for configuration-driven selection.
    #[tokio::test]
    async fn crate_exposes_multiple_backends_for_configuration_selection() {
        use crate::OpenAiProvider;

        // Construct both providers — if either type is missing or not
        // exported, this test fails to compile.
        let anthropic: Box<dyn Provider> = Box::new(AnthropicProvider::new(
            "test-anthropic-key",
            "claude-sonnet-4-20250514",
        ));
        let openai: Box<dyn Provider> = Box::new(OpenAiProvider::new("test-openai-key", "gpt-4o"));

        // Exercise each provider with an exhausted budget to confirm the
        // trait method dispatches correctly (budget check happens before
        // any HTTP call, so no fake HTTP client is needed).
        let messages = vec![Message {
            role: simulacra_types::Role::User,
            content: "hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }];

        let mut budget_a = exhausted_budget();
        let err_a = anthropic
            .chat(&messages, &[], &mut budget_a)
            .await
            .expect_err("Anthropic provider should reject exhausted budget");
        assert!(
            matches!(err_a, ProviderError::BudgetExhausted(_)),
            "Anthropic backend should return BudgetExhausted, got: {err_a:?}"
        );

        let mut budget_o = exhausted_budget();
        let err_o = openai
            .chat(&messages, &[], &mut budget_o)
            .await
            .expect_err("OpenAI provider should reject exhausted budget");
        assert!(
            matches!(err_o, ProviderError::BudgetExhausted(_)),
            "OpenAI backend should return BudgetExhausted, got: {err_o:?}"
        );
    }

    // ── S010: OTel GenAI Semantic Convention Tests ────────────────

    mod otel_span_tests {
        use super::*;
        use std::sync::Mutex;
        use tracing_subscriber::layer::SubscriberExt;

        /// Captured span data for test assertions.
        #[derive(Debug, Clone)]
        struct CapturedSpan {
            name: String,
            fields: std::collections::HashMap<String, String>,
        }

        #[derive(Debug, Clone)]
        struct CapturedEvent {
            #[allow(dead_code)]
            name: String,
            level: String,
            current_span: Option<String>,
            fields: std::collections::HashMap<String, String>,
        }

        /// A tracing Layer that captures span names and field values.
        struct SpanCaptureLayer {
            spans: Arc<Mutex<Vec<CapturedSpan>>>,
        }

        impl<S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>
            tracing_subscriber::Layer<S> for SpanCaptureLayer
        {
            fn on_new_span(
                &self,
                attrs: &tracing::span::Attributes<'_>,
                _id: &tracing::span::Id,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = std::collections::HashMap::new();
                let mut visitor = FieldVisitor(&mut fields);
                attrs.record(&mut visitor);
                let span = CapturedSpan {
                    name: attrs.metadata().name().to_string(),
                    fields,
                };
                self.spans.lock().unwrap().push(span);
            }

            fn on_record(
                &self,
                id: &tracing::span::Id,
                values: &tracing::span::Record<'_>,
                ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                // Update existing span with newly recorded fields.
                let span_ref = ctx.span(id);
                if let Some(span_ref) = span_ref {
                    let span_name = span_ref.name().to_string();
                    let mut new_fields = std::collections::HashMap::new();
                    let mut visitor = FieldVisitor(&mut new_fields);
                    values.record(&mut visitor);
                    let mut spans = self.spans.lock().unwrap();
                    // Find the matching span and merge fields
                    for captured in spans.iter_mut().rev() {
                        if captured.name == span_name {
                            for (k, v) in new_fields {
                                captured.fields.insert(k, v);
                            }
                            break;
                        }
                    }
                }
            }
        }

        struct FieldVisitor<'a>(&'a mut std::collections::HashMap<String, String>);

        impl tracing::field::Visit for FieldVisitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
            fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
        }

        fn setup_span_capture() -> (
            impl tracing::Subscriber + Send + Sync,
            Arc<Mutex<Vec<CapturedSpan>>>,
        ) {
            let spans = Arc::new(Mutex::new(Vec::new()));
            let layer = SpanCaptureLayer {
                spans: Arc::clone(&spans),
            };
            let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
            (subscriber, spans)
        }

        struct TraceCaptureLayer {
            spans: Arc<Mutex<Vec<CapturedSpan>>>,
            events: Arc<Mutex<Vec<CapturedEvent>>>,
        }

        impl<S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>
            tracing_subscriber::Layer<S> for TraceCaptureLayer
        {
            fn on_new_span(
                &self,
                attrs: &tracing::span::Attributes<'_>,
                _id: &tracing::span::Id,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = std::collections::HashMap::new();
                let mut visitor = FieldVisitor(&mut fields);
                attrs.record(&mut visitor);
                self.spans.lock().unwrap().push(CapturedSpan {
                    name: attrs.metadata().name().to_string(),
                    fields,
                });
            }

            fn on_record(
                &self,
                id: &tracing::span::Id,
                values: &tracing::span::Record<'_>,
                ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if let Some(span_ref) = ctx.span(id) {
                    let span_name = span_ref.name().to_string();
                    let mut new_fields = std::collections::HashMap::new();
                    let mut visitor = FieldVisitor(&mut new_fields);
                    values.record(&mut visitor);

                    let mut spans = self.spans.lock().unwrap();
                    for captured in spans.iter_mut().rev() {
                        if captured.name == span_name {
                            captured.fields.extend(new_fields);
                            break;
                        }
                    }
                }
            }

            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = std::collections::HashMap::new();
                let mut visitor = FieldVisitor(&mut fields);
                event.record(&mut visitor);
                self.events.lock().unwrap().push(CapturedEvent {
                    name: event.metadata().name().to_string(),
                    level: event.metadata().level().to_string(),
                    current_span: ctx.lookup_current().map(|span| span.name().to_string()),
                    fields,
                });
            }
        }

        #[allow(clippy::type_complexity)]
        fn setup_trace_capture() -> (
            impl tracing::Subscriber + Send + Sync,
            Arc<Mutex<Vec<CapturedSpan>>>,
            Arc<Mutex<Vec<CapturedEvent>>>,
        ) {
            let spans = Arc::new(Mutex::new(Vec::new()));
            let events = Arc::new(Mutex::new(Vec::new()));
            let subscriber =
                tracing_subscriber::registry::Registry::default().with(TraceCaptureLayer {
                    spans: Arc::clone(&spans),
                    events: Arc::clone(&events),
                });
            (subscriber, spans, events)
        }

        // S010 Assertion: Every LLM call produces a span with all required attributes
        #[tokio::test]
        async fn chat_emits_span_with_required_gen_ai_attributes() {
            let (subscriber, captured) = setup_span_capture();
            let fake = FakeHttpClient::with_response(200, &success_response_json());
            let provider = AnthropicProvider::with_http_client(
                "test-key",
                "claude-sonnet-4-20250514",
                Box::new(fake),
            );

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let _resp = provider.chat(&messages, &[], &mut budget).await.unwrap();

            let spans = captured.lock().unwrap();
            let chat_span = spans
                .iter()
                .find(|s| s.fields.get("gen_ai.operation.name") == Some(&"chat".to_string()))
                .expect("expected a span with gen_ai.operation.name=chat");

            // Span name contains the operation name
            assert!(
                chat_span.name.contains("chat"),
                "span name should contain 'chat', got: {}",
                chat_span.name
            );

            // Required attributes from S010
            assert_eq!(
                chat_span.fields.get("gen_ai.request.model"),
                Some(&"claude-sonnet-4-20250514".to_string())
            );
            assert_eq!(
                chat_span.fields.get("gen_ai.provider.name"),
                Some(&"anthropic".to_string())
            );
            assert_eq!(
                chat_span.fields.get("server.address"),
                Some(&"api.anthropic.com".to_string())
            );
            assert_eq!(
                chat_span.fields.get("server.port"),
                Some(&"443".to_string())
            );
            assert_eq!(
                chat_span.fields.get("gen_ai.request.max_tokens"),
                Some(&"8192".to_string())
            );
        }

        #[tokio::test]
        async fn chat_span_sets_otel_name_to_chat_and_model() {
            let (subscriber, captured) = setup_span_capture();
            let model = "claude-sonnet-4-20250514";
            let fake = FakeHttpClient::with_response(200, &success_response_json());
            let provider = AnthropicProvider::with_http_client("test-key", model, Box::new(fake));

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = provider.chat(&messages, &[], &mut budget).await.unwrap();

            let spans = captured.lock().unwrap();
            let chat_span = spans
                .iter()
                .find(|s| s.fields.get("gen_ai.operation.name") == Some(&"chat".to_string()))
                .expect("expected a span with gen_ai.operation.name=chat");

            assert_eq!(
                chat_span.fields.get("otel.name"),
                Some(&format!("chat {model}")),
                "LLM spans should expose the exact chat {{model}} name via otel.name"
            );
        }

        // S010 Assertion: gen_ai.provider.name matches the actual provider
        #[tokio::test]
        async fn provider_name_is_anthropic() {
            let (subscriber, captured) = setup_span_capture();
            let fake = FakeHttpClient::with_response(200, &success_response_json());
            let provider = AnthropicProvider::with_http_client(
                "test-key",
                "claude-sonnet-4-20250514",
                Box::new(fake),
            );

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let _resp = provider.chat(&messages, &[], &mut budget).await.unwrap();

            let spans = captured.lock().unwrap();
            let chat_span = spans
                .iter()
                .find(|s| s.fields.get("gen_ai.operation.name") == Some(&"chat".to_string()))
                .expect("expected a span with gen_ai.operation.name=chat");
            assert_eq!(
                chat_span.fields.get("gen_ai.provider.name"),
                Some(&"anthropic".to_string())
            );
        }

        // S010 Assertion: Token counts match provider API values
        #[tokio::test]
        async fn token_counts_recorded_on_span() {
            let (subscriber, captured) = setup_span_capture();
            let fake = FakeHttpClient::with_response(200, &success_response_json());
            let provider = AnthropicProvider::with_http_client(
                "test-key",
                "claude-sonnet-4-20250514",
                Box::new(fake),
            );

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let _resp = provider.chat(&messages, &[], &mut budget).await.unwrap();

            let spans = captured.lock().unwrap();
            let chat_span = spans
                .iter()
                .find(|s| s.fields.get("gen_ai.operation.name") == Some(&"chat".to_string()))
                .expect("expected a span with gen_ai.operation.name=chat");

            // success_response_json has input_tokens=10, output_tokens=25
            assert_eq!(
                chat_span.fields.get("gen_ai.usage.input_tokens"),
                Some(&"10".to_string())
            );
            assert_eq!(
                chat_span.fields.get("gen_ai.usage.output_tokens"),
                Some(&"25".to_string())
            );
        }

        // S010 Assertion: response ID and finish reason recorded
        #[tokio::test]
        async fn response_id_and_finish_reason_recorded_on_span() {
            let (subscriber, captured) = setup_span_capture();
            let fake = FakeHttpClient::with_response(200, &success_response_json());
            let provider = AnthropicProvider::with_http_client(
                "test-key",
                "claude-sonnet-4-20250514",
                Box::new(fake),
            );

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let _resp = provider.chat(&messages, &[], &mut budget).await.unwrap();

            let spans = captured.lock().unwrap();
            let chat_span = spans
                .iter()
                .find(|s| s.fields.get("gen_ai.operation.name") == Some(&"chat".to_string()))
                .expect("expected a span with gen_ai.operation.name=chat");

            assert_eq!(
                chat_span.fields.get("gen_ai.response.id"),
                Some(&"msg_test123".to_string())
            );
            assert_eq!(
                chat_span.fields.get("gen_ai.response.finish_reasons"),
                Some(&"[\"end_turn\"]".to_string())
            );
        }

        #[tokio::test]
        async fn failed_provider_call_still_emits_chat_span() {
            // Edge case: failed provider calls should still produce a chat span so observability
            // isn't lost on the error path.
            let (subscriber, captured) = setup_span_capture();
            let error_body = serde_json::to_vec(&serde_json::json!({
                "type": "error",
                "error": {"type": "api_error", "message": "internal server error"}
            }))
            .unwrap();
            let fake = FakeHttpClient::with_response(500, &error_body);
            let provider = AnthropicProvider::with_http_client(
                "test-key",
                "claude-sonnet-4-20250514",
                Box::new(fake),
            );

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let err = provider
                .chat(&messages, &[], &mut budget)
                .await
                .unwrap_err();
            assert!(matches!(err, ProviderError::ServerError(_)));

            let spans = captured.lock().unwrap();
            let chat_span = spans
                .iter()
                .find(|s| s.fields.get("gen_ai.operation.name") == Some(&"chat".to_string()))
                .expect("expected a chat span even when the call fails");

            assert_eq!(
                chat_span.fields.get("gen_ai.request.model"),
                Some(&"claude-sonnet-4-20250514".to_string())
            );
            assert_eq!(
                chat_span.fields.get("gen_ai.provider.name"),
                Some(&"anthropic".to_string())
            );
        }

        #[tokio::test]
        async fn token_usage_attributes_are_recorded_as_numeric_fields() {
            // Edge case: token usage should be emitted as numeric span fields, not stringly typed
            // attributes that only look numeric after formatting.
            #[derive(Debug, Clone, PartialEq, Eq)]
            enum CapturedValue {
                U64(u64),
                Str(String),
                Debug(String),
            }

            #[derive(Debug, Clone)]
            struct TypedSpan {
                name: String,
                fields: std::collections::HashMap<String, CapturedValue>,
            }

            struct TypedCaptureLayer {
                spans: Arc<Mutex<Vec<TypedSpan>>>,
            }

            struct TypedFieldVisitor<'a>(&'a mut std::collections::HashMap<String, CapturedValue>);

            impl tracing::field::Visit for TypedFieldVisitor<'_> {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.insert(
                        field.name().to_string(),
                        CapturedValue::Debug(format!("{value:?}")),
                    );
                }

                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.insert(
                        field.name().to_string(),
                        CapturedValue::Str(value.to_string()),
                    );
                }

                fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                    self.0
                        .insert(field.name().to_string(), CapturedValue::U64(value));
                }
            }

            impl<S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>
                tracing_subscriber::Layer<S> for TypedCaptureLayer
            {
                fn on_new_span(
                    &self,
                    attrs: &tracing::span::Attributes<'_>,
                    _id: &tracing::span::Id,
                    _ctx: tracing_subscriber::layer::Context<'_, S>,
                ) {
                    let mut fields = std::collections::HashMap::new();
                    let mut visitor = TypedFieldVisitor(&mut fields);
                    attrs.record(&mut visitor);
                    self.spans.lock().unwrap().push(TypedSpan {
                        name: attrs.metadata().name().to_string(),
                        fields,
                    });
                }

                fn on_record(
                    &self,
                    id: &tracing::span::Id,
                    values: &tracing::span::Record<'_>,
                    ctx: tracing_subscriber::layer::Context<'_, S>,
                ) {
                    if let Some(span_ref) = ctx.span(id) {
                        let name = span_ref.name().to_string();
                        let mut new_fields = std::collections::HashMap::new();
                        let mut visitor = TypedFieldVisitor(&mut new_fields);
                        values.record(&mut visitor);

                        let mut spans = self.spans.lock().unwrap();
                        for span in spans.iter_mut().rev() {
                            if span.name == name {
                                span.fields.extend(new_fields);
                                break;
                            }
                        }
                    }
                }
            }

            let spans = Arc::new(Mutex::new(Vec::new()));
            let subscriber =
                tracing_subscriber::registry::Registry::default().with(TypedCaptureLayer {
                    spans: Arc::clone(&spans),
                });
            let fake = FakeHttpClient::with_response(200, &success_response_json());
            let provider = AnthropicProvider::with_http_client(
                "test-key",
                "claude-sonnet-4-20250514",
                Box::new(fake),
            );

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = provider.chat(&messages, &[], &mut budget).await.unwrap();

            let spans = spans.lock().unwrap();
            let chat_span = spans
                .iter()
                .find(|span| span.name == "chat")
                .expect("expected chat span");

            assert_eq!(
                chat_span.fields.get("gen_ai.usage.input_tokens"),
                Some(&CapturedValue::U64(10))
            );
            assert_eq!(
                chat_span.fields.get("gen_ai.usage.output_tokens"),
                Some(&CapturedValue::U64(25))
            );
        }

        #[tokio::test]
        async fn finish_reasons_use_snake_case_array_format_for_end_turn_and_tool_use() {
            // Edge case: finish reasons should be recorded in the OTel array-string format, with
            // provider values like end_turn and tool_use preserved in snake_case.
            let (subscriber, captured) = setup_span_capture();
            let end_turn = AnthropicProvider::with_http_client(
                "test-key",
                "claude-sonnet-4-20250514",
                Box::new(FakeHttpClient::with_response(200, &success_response_json())),
            );
            let tool_use = AnthropicProvider::with_http_client(
                "test-key",
                "claude-sonnet-4-20250514",
                Box::new(FakeHttpClient::with_response(
                    200,
                    &tool_use_response_json(),
                )),
            );

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = end_turn.chat(&messages, &[], &mut budget).await.unwrap();
            let _ = tool_use.chat(&messages, &[], &mut budget).await.unwrap();

            let spans = captured.lock().unwrap();
            let end_turn_span = spans
                .iter()
                .find(|span| span.fields.get("gen_ai.response.id") == Some(&"msg_test123".into()))
                .expect("expected end_turn span");
            let tool_use_span = spans
                .iter()
                .find(|span| span.fields.get("gen_ai.response.id") == Some(&"msg_tool456".into()))
                .expect("expected tool_use span");

            assert_eq!(
                end_turn_span.fields.get("gen_ai.response.finish_reasons"),
                Some(&"[\"end_turn\"]".to_string())
            );
            assert_eq!(
                tool_use_span.fields.get("gen_ai.response.finish_reasons"),
                Some(&"[\"tool_use\"]".to_string())
            );
        }

        // S010 Assertion: No Simulacra-specific attributes use gen_ai.* namespace
        #[tokio::test]
        async fn no_simulacra_specific_attributes_in_gen_ai_namespace() {
            let (subscriber, captured) = setup_span_capture();
            let fake = FakeHttpClient::with_response(200, &success_response_json());
            let provider = AnthropicProvider::with_http_client(
                "test-key",
                "claude-sonnet-4-20250514",
                Box::new(fake),
            );

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let _resp = provider.chat(&messages, &[], &mut budget).await.unwrap();

            let spans = captured.lock().unwrap();
            // All gen_ai.* attributes should be from the OTel GenAI spec, not Simulacra-specific.
            // Known allowed gen_ai.* keys from the spec:
            let allowed_gen_ai_keys = [
                "gen_ai.operation.name",
                "gen_ai.request.model",
                "gen_ai.provider.name",
                "gen_ai.usage.input_tokens",
                "gen_ai.usage.output_tokens",
                "gen_ai.request.max_tokens",
                "gen_ai.request.temperature",
                "gen_ai.response.id",
                "gen_ai.response.finish_reasons",
                "gen_ai.agent.name",
                "gen_ai.tool.message",
            ];
            for span in spans.iter() {
                for key in span.fields.keys() {
                    if key.starts_with("gen_ai.") {
                        assert!(
                            allowed_gen_ai_keys.contains(&key.as_str()),
                            "found Simulacra-specific attribute in gen_ai.* namespace: {key}"
                        );
                    }
                }
            }
        }

        #[tokio::test]
        async fn token_usage_histogram_is_recorded_with_operation_and_model_labels() {
            let (subscriber, _spans, captured_events) = setup_trace_capture();
            let model = "claude-sonnet-4-20250514";
            let fake = FakeHttpClient::with_response(200, &success_response_json());
            let provider = AnthropicProvider::with_http_client("test-key", model, Box::new(fake));

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let response = provider.chat(&messages, &[], &mut budget).await.unwrap();

            let events = captured_events.lock().unwrap();
            assert!(
                events.iter().any(|event| {
                    event.fields.get("gen_ai.client.token.usage")
                        == Some(&response.token_usage.total().to_string())
                        && event.fields.get("operation") == Some(&"chat".to_string())
                        && event.fields.get("model") == Some(&model.to_string())
                }),
                "expected gen_ai.client.token.usage histogram event tagged with operation=chat and the exact model"
            );
        }

        #[tokio::test]
        async fn operation_duration_histogram_is_recorded_with_operation_and_model_labels() {
            let (subscriber, _spans, captured_events) = setup_trace_capture();
            let model = "claude-sonnet-4-20250514";
            let fake = FakeHttpClient::with_response(200, &success_response_json());
            let provider = AnthropicProvider::with_http_client("test-key", model, Box::new(fake));

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = provider.chat(&messages, &[], &mut budget).await.unwrap();

            let events = captured_events.lock().unwrap();
            assert!(
                events.iter().any(|event| {
                    event
                        .fields
                        .get("gen_ai.client.operation.duration")
                        .is_some_and(|value| {
                            value.parse::<f64>().is_ok_and(|duration| duration >= 0.0)
                        })
                        && event.fields.get("operation") == Some(&"chat".to_string())
                        && event.fields.get("model") == Some(&model.to_string())
                }),
                "expected gen_ai.client.operation.duration histogram event tagged with operation=chat and the exact model"
            );
        }

        #[tokio::test]
        async fn tool_call_counter_increments_per_returned_tool_call_with_name_and_source() {
            let (subscriber, _spans, captured_events) = setup_trace_capture();
            let fake = FakeHttpClient::with_response(200, &tool_use_response_json());
            let provider = AnthropicProvider::with_http_client(
                "test-key",
                "claude-sonnet-4-20250514",
                Box::new(fake),
            );

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "What's the weather?".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let tools = vec![ToolDefinition {
                name: "get_weather".into(),
                description: "Get weather for a location".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    }
                }),
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = provider.chat(&messages, &tools, &mut budget).await.unwrap();

            let events = captured_events.lock().unwrap();
            assert!(
                events.iter().any(|event| {
                    event.fields.get("simulacra.tool.calls") == Some(&"1".to_string())
                        && event.fields.get("tool_name") == Some(&"get_weather".to_string())
                        && event.fields.get("source") == Some(&"builtin".to_string())
                }),
                "expected simulacra.tool.calls counter event for each returned tool call with tool_name and source labels"
            );
        }

        #[tokio::test]
        async fn retryable_provider_errors_are_logged_at_warn_with_error_details() {
            let (subscriber, _spans, captured_events) = setup_trace_capture();
            let mut headers = HashMap::new();
            headers.insert("retry-after".to_owned(), "30".to_owned());
            let error_body = serde_json::to_vec(&serde_json::json!({
                "type": "error",
                "error": {"type": "rate_limit_error", "message": "too many requests"}
            }))
            .unwrap();
            let fake = FakeHttpClient::with_response_and_headers(429, &error_body, headers);
            let provider = AnthropicProvider::with_http_client(
                "test-key",
                "claude-sonnet-4-20250514",
                Box::new(fake),
            );

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let err = provider
                .chat(&messages, &[], &mut budget)
                .await
                .expect_err("retryable provider failure should still return an error");
            assert!(err.is_retryable());

            let events = captured_events.lock().unwrap();
            assert!(
                events.iter().any(|event| {
                    event.level == "WARN"
                        && event.current_span.as_deref() == Some("chat")
                        && event.fields.values().any(|value| {
                            value.contains("too many requests") || value.contains("rate limited")
                        })
                }),
                "expected a WARN provider error event on the chat span with the retryable error details"
            );
        }

        #[tokio::test]
        async fn non_retryable_provider_errors_are_logged_at_error_with_error_details() {
            let (subscriber, _spans, captured_events) = setup_trace_capture();
            let error_body = serde_json::to_vec(&serde_json::json!({
                "type": "error",
                "error": {"type": "authentication_error", "message": "invalid api key"}
            }))
            .unwrap();
            let fake = FakeHttpClient::with_response(401, &error_body);
            let provider = AnthropicProvider::with_http_client(
                "test-key",
                "claude-sonnet-4-20250514",
                Box::new(fake),
            );

            let messages = vec![Message {
                role: simulacra_types::Role::User,
                content: "Hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }];
            let mut budget = fresh_budget();

            let _guard = tracing::subscriber::set_default(subscriber);
            let err = provider
                .chat(&messages, &[], &mut budget)
                .await
                .expect_err("non-retryable provider failure should still return an error");
            assert!(!err.is_retryable());

            let events = captured_events.lock().unwrap();
            assert!(
                events.iter().any(|event| {
                    event.level == "ERROR"
                        && event.current_span.as_deref() == Some("chat")
                        && event.fields.values().any(|value| {
                            value.contains("invalid api key") || value.contains("authentication")
                        })
                }),
                "expected an ERROR provider error event on the chat span with the non-retryable error details"
            );
        }
    }
}
