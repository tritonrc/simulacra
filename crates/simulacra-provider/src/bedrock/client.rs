//! BedrockProvider implementation (SKELETON — behavior stubbed for RED phase).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::Instrument;

use simulacra_types::{
    Message, Provider, ProviderError, ProviderResponse, ResourceBudget, StreamingProvider,
    ToolDefinition,
};

use crate::bedrock::sigv4::Credentials;

// ── HTTP abstraction (mirrors the Anthropic/OpenAI providers) ───────

/// Minimal HTTP client trait so tests can substitute a fake.
pub(crate) trait HttpClient: Send + Sync {
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + '_>>;

    fn post_stream<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a [u8],
        sink: &'a mut dyn HttpStreamSink,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            let response = self.post(url, headers, body).await?;
            sink.begin(response.status, &response.headers)?;
            sink.chunk(&response.body)?;
            Ok(response)
        })
    }
}

pub(crate) trait HttpStreamSink: Send {
    fn begin(
        &mut self,
        _status: u16,
        _headers: &HashMap<String, String>,
    ) -> Result<(), ProviderError> {
        Ok(())
    }
    fn chunk(&mut self, _chunk: &[u8]) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Raw HTTP response.
pub(crate) struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// Reqwest-backed HTTP client (used in production).
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
                .map_err(|e| ProviderError::Other(format!("failed to read response body: {e}")))?
                .to_vec();

            Ok(HttpResponse {
                status,
                headers: resp_headers,
                body: resp_body,
            })
        })
    }

    fn post_stream<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a [u8],
        sink: &'a mut dyn HttpStreamSink,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + 'a>> {
        let url = url.to_owned();
        let headers = headers.to_vec();
        let body = body.to_vec();
        Box::pin(async move {
            let mut builder = self.client.post(&url);
            for (key, value) in &headers {
                builder = builder.header(key.as_str(), value.as_str());
            }
            let mut resp = builder
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
            sink.begin(status, &resp_headers)?;

            let mut resp_body = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| ProviderError::Other(format!("failed to read response chunk: {e}")))?
            {
                resp_body.extend_from_slice(&chunk);
                sink.chunk(&chunk)?;
            }

            Ok(HttpResponse {
                status: resp.status().as_u16(),
                headers: resp_headers,
                body: resp_body,
            })
        })
    }
}

// ── BedrockProvider ────────────────────────────────────────────────

const SERVICE: &str = "bedrock";

/// AWS Bedrock (Converse API) provider.
pub struct BedrockProvider {
    region: String,
    model: String,
    credentials: Credentials,
    /// Optional base URL override (used by tests to point at a fake upstream).
    base_url: Option<String>,
    http: Box<dyn HttpClient>,
}

impl BedrockProvider {
    /// Create a provider, reading credentials from the standard AWS
    /// environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
    /// optional `AWS_SESSION_TOKEN`) and region from `AWS_REGION` /
    /// `AWS_DEFAULT_REGION`.
    pub fn new(region: impl Into<String>, model: impl Into<String>) -> Self {
        let region = region.into();
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default();
        let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default();
        let session_token = std::env::var("AWS_SESSION_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        Self::with_credentials(
            access_key_id,
            secret_access_key,
            session_token,
            region,
            model,
        )
    }

    /// Create a provider with explicit credentials.
    pub fn with_credentials(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        session_token: Option<String>,
        region: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            region: region.into(),
            model: model.into(),
            credentials: Credentials {
                access_key_id: access_key_id.into().trim().to_owned(),
                secret_access_key: secret_access_key.into().trim().to_owned(),
                session_token: session_token.map(|t| t.trim().to_owned()),
            },
            base_url: std::env::var("BEDROCK_BASE_URL").ok(),
            http: Box::new(ReqwestClient::new()),
        }
    }

    /// Inject a custom HTTP client (tests only).
    #[cfg(test)]
    pub(crate) fn with_http_client(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        region: impl Into<String>,
        model: impl Into<String>,
        http: Box<dyn HttpClient>,
    ) -> Self {
        Self {
            region: region.into(),
            model: model.into(),
            credentials: Credentials {
                access_key_id: access_key_id.into(),
                secret_access_key: secret_access_key.into(),
                session_token: None,
            },
            base_url: None,
            http,
        }
    }

    /// Configured model id.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Configured region.
    pub fn region(&self) -> &str {
        &self.region
    }
}

// ── OTel meters ──────────────────────────────────────────────────

/// Lazily-initialized OTel meter instruments for the Bedrock provider.
struct BedrockMeters {
    duration_histogram: opentelemetry::metrics::Histogram<f64>,
    token_usage_histogram: opentelemetry::metrics::Histogram<u64>,
}

impl BedrockMeters {
    fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<BedrockMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-provider");
            BedrockMeters {
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

// ── Time / date helpers (no chrono dependency) ─────────────────────

/// Current UTC time as `(amz_date = YYYYMMDDTHHMMSSZ, date_stamp = YYYYMMDD)`.
fn amz_date_and_date_stamp() -> (String, String) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64;
    let sod = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    let hh = sod / 3600;
    let mm = (sod % 3600) / 60;
    let ss = sod % 60;
    (
        format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z"),
        format!("{y:04}{m:02}{d:02}"),
    )
}

/// Convert days-since-Unix-epoch to a proleptic Gregorian (year, month, day).
/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ── Request signing / URL building ─────────────────────────────────

impl BedrockProvider {
    /// The API host. Defaults to the regional Bedrock Runtime endpoint;
    /// derived from `BEDROCK_BASE_URL` (origin only) when set.
    fn host(&self) -> String {
        if let Some(base) = &self.base_url {
            if let Some(rest) = base
                .strip_prefix("https://")
                .or_else(|| base.strip_prefix("http://"))
            {
                return rest.trim_end_matches('/').to_owned();
            }
            return base.trim_end_matches('/').to_owned();
        }
        format!("bedrock-runtime.{}.amazonaws.com", self.region)
    }

    /// Build the signed request headers + URL for a converse endpoint.
    fn build_signed_request(
        &self,
        path_suffix: &str,
        body: Vec<u8>,
        span_model: &str,
        max_tokens: u32,
    ) -> Result<(String, Vec<(String, String)>), ProviderError> {
        use crate::bedrock::sigv4::{SigningRequest, SigningTarget, sign, uri_encode};

        let host = self.host();
        let encoded_model = uri_encode(&self.model, false);
        let path = format!("/model/{encoded_model}/{path_suffix}");
        let scheme = if self
            .base_url
            .as_deref()
            .is_some_and(|b| b.starts_with("http://"))
        {
            "http"
        } else {
            "https"
        };
        let url = format!("{scheme}://{host}{path}");

        let (amz_date, date_stamp) = amz_date_and_date_stamp();
        let signed = sign(SigningRequest {
            credentials: &self.credentials,
            region: &self.region,
            service: SERVICE,
            amz_date: &amz_date,
            date_stamp: &date_stamp,
            target: &SigningTarget {
                host: host.clone(),
                path: path.clone(),
            },
            body: &body,
        });

        let mut headers: Vec<(String, String)> =
            vec![("content-type".to_owned(), "application/json".to_owned())];
        headers.push(("authorization".to_owned(), signed.authorization));
        headers.extend(signed.extra);

        // Suppress unused-binding warnings on OTel span fields; the span is
        // created at the call site below.
        let _ = (span_model, max_tokens);

        Ok((url, headers))
    }
}

/// Map a non-200 Bedrock response into a typed `ProviderError`, surfacing the
/// `message` field from the `{"message": "..."}` error body.
fn classify_bedrock_error(
    status: u16,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> ProviderError {
    let message = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
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

/// Emit the post-response OTel attributes/meters shared by chat + chat_stream.
fn record_telemetry(provider_resp: &ProviderResponse, call_start: std::time::Instant, model: &str) {
    use opentelemetry::KeyValue;

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
        gen_ai.request.model = model,
        operation = "chat",
        model = model,
        "token usage"
    );
    let duration_secs = call_start.elapsed().as_secs_f64();
    tracing::info!(
        gen_ai.client.operation.duration = duration_secs,
        gen_ai.operation.name = "chat",
        gen_ai.request.model = model,
        operation = "chat",
        model = model,
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

    let meters = BedrockMeters::get();
    let base = &[
        KeyValue::new("gen_ai.operation.name", "chat"),
        KeyValue::new("gen_ai.request.model", model.to_owned()),
        KeyValue::new("gen_ai.provider.name", "bedrock"),
    ];
    meters
        .duration_histogram
        .record(call_start.elapsed().as_secs_f64() * 1000.0, base);
    meters.token_usage_histogram.record(
        provider_resp.token_usage.input_tokens,
        &[
            KeyValue::new("gen_ai.operation.name", "chat"),
            KeyValue::new("gen_ai.request.model", model.to_owned()),
            KeyValue::new("gen_ai.provider.name", "bedrock"),
            KeyValue::new("gen_ai.token.type", "input"),
        ],
    );
    meters.token_usage_histogram.record(
        provider_resp.token_usage.output_tokens,
        &[
            KeyValue::new("gen_ai.operation.name", "chat"),
            KeyValue::new("gen_ai.request.model", model.to_owned()),
            KeyValue::new("gen_ai.provider.name", "bedrock"),
            KeyValue::new("gen_ai.token.type", "output"),
        ],
    );
}

/// Derive `maxTokens` from the remaining token budget (0 = unlimited → 8192).
fn derive_max_tokens(budget: &ResourceBudget) -> u32 {
    if budget.max_tokens == 0 {
        8192
    } else {
        let remaining = budget.max_tokens.saturating_sub(budget.used_tokens);
        (remaining.min(8192) as u32).max(1)
    }
}

/// Log + classify a non-200 response, returning the typed error.
fn handle_error_response(
    status: u16,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> ProviderError {
    let err = classify_bedrock_error(status, headers, body);
    if err.is_retryable() {
        tracing::warn!(
            error_type = "server_error",
            status,
            "provider error: retryable"
        );
    } else {
        tracing::error!(
            error_type = "client_error",
            status,
            "provider error: non-retryable"
        );
    }
    err
}

// ── converse-stream emitter ────────────────────────────────────────

/// Bridges the raw HTTP byte stream into decoded frames fed to the
/// accumulator, while forwarding provider stream events to the sink.
struct BedrockStreamEmitter<'a> {
    decoder: crate::bedrock::eventstream::BedrockEventStreamDecoder,
    accumulator: crate::bedrock::api_types::ConverseStreamAccumulator<'a>,
    active: bool,
}

impl<'a> BedrockStreamEmitter<'a> {
    fn new(model: &str, sink: &'a dyn simulacra_types::ProviderStreamSink) -> Self {
        Self {
            decoder: crate::bedrock::eventstream::BedrockEventStreamDecoder::new(),
            accumulator: crate::bedrock::api_types::ConverseStreamAccumulator::new(
                model,
                Some(sink),
                None,
            ),
            active: false,
        }
    }

    fn finish(self) -> ProviderResponse {
        self.accumulator.finish()
    }
}

impl HttpStreamSink for BedrockStreamEmitter<'_> {
    fn begin(
        &mut self,
        status: u16,
        headers: &HashMap<String, String>,
    ) -> Result<(), ProviderError> {
        self.active = status == 200
            && headers
                .get("content-type")
                .is_some_and(|ct| ct.contains("application/vnd.amazon.eventstream"));
        if self.active
            && let Some(id) = headers.get("x-amz-request-id")
        {
            self.accumulator.set_response_id(Some(id.clone()));
        }
        Ok(())
    }

    fn chunk(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        if !self.active {
            return Ok(());
        }
        let frames = self.decoder.push_bytes(chunk)?;
        for frame in &frames {
            self.accumulator.process_frame(frame)?;
        }
        Ok(())
    }
}

// ── Provider impls ─────────────────────────────────────────────────

impl Provider for BedrockProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        budget: &'a mut ResourceBudget,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>> {
        use crate::bedrock::api_types::{build_request_body, parse_json_response};

        // S007: budget is checked BEFORE any HTTP call.
        if let Err(e) = budget.check_budget() {
            return Box::pin(async move { Err(ProviderError::BudgetExhausted(e)) });
        }

        let max_tokens = derive_max_tokens(budget);
        let request_body = build_request_body(messages, tools, &self.model, max_tokens);
        let body = match serde_json::to_vec(&request_body) {
            Ok(b) => b,
            Err(e) => {
                return Box::pin(async move {
                    Err(ProviderError::Other(format!(
                        "request serialization failed: {e}"
                    )))
                });
            }
        };

        let (url, headers) = match self.build_signed_request(
            "converse",
            body.clone(),
            &self.model.clone(),
            max_tokens,
        ) {
            Ok(v) => v,
            Err(e) => return Box::pin(async move { Err(e) }),
        };

        let http = &*self.http;
        let model = self.model.clone();
        let otel_name = format!("chat {model}");
        let host = self.host();
        let span = tracing::info_span!(
            "chat",
            "otel.name" = otel_name.as_str(),
            "gen_ai.operation.name" = "chat",
            "gen_ai.request.model" = model.as_str(),
            "gen_ai.provider.name" = "bedrock",
            "gen_ai.request.max_tokens" = max_tokens as u64,
            "server.address" = host.as_str(),
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "gen_ai.response.id" = tracing::field::Empty,
        );

        let fut = async move {
            let call_start = std::time::Instant::now();
            let resp = http.post(&url, &headers, &body).await?;

            if resp.status != 200 {
                return Err(handle_error_response(
                    resp.status,
                    &resp.headers,
                    &resp.body,
                ));
            }

            let response_id = resp.headers.get("x-amz-request-id").cloned();
            let provider_resp = parse_json_response(&resp.body, &model, response_id)?;
            record_telemetry(&provider_resp, call_start, &model);
            Ok(provider_resp)
        };
        Box::pin(fut.instrument(span))
    }

    fn as_streaming(&self) -> Option<&dyn StreamingProvider> {
        Some(self)
    }
}

impl StreamingProvider for BedrockProvider {
    fn chat_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        budget: &'a mut ResourceBudget,
        stream_sink: &'a dyn simulacra_types::ProviderStreamSink,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>> {
        use crate::bedrock::api_types::{build_request_body, parse_json_response};

        if let Err(e) = budget.check_budget() {
            return Box::pin(async move { Err(ProviderError::BudgetExhausted(e)) });
        }

        let max_tokens = derive_max_tokens(budget);
        let request_body = build_request_body(messages, tools, &self.model, max_tokens);
        let body = match serde_json::to_vec(&request_body) {
            Ok(b) => b,
            Err(e) => {
                return Box::pin(async move {
                    Err(ProviderError::Other(format!(
                        "request serialization failed: {e}"
                    )))
                });
            }
        };

        let (url, headers) = match self.build_signed_request(
            "converse-stream",
            body.clone(),
            &self.model.clone(),
            max_tokens,
        ) {
            Ok(v) => v,
            Err(e) => return Box::pin(async move { Err(e) }),
        };

        let http = &*self.http;
        let model = self.model.clone();
        let otel_name = format!("chat {model}");
        let host = self.host();
        let span = tracing::info_span!(
            "chat",
            "otel.name" = otel_name.as_str(),
            "gen_ai.operation.name" = "chat",
            "gen_ai.request.model" = model.as_str(),
            "gen_ai.provider.name" = "bedrock",
            "gen_ai.request.max_tokens" = max_tokens as u64,
            "server.address" = host.as_str(),
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "gen_ai.response.id" = tracing::field::Empty,
        );

        let fut = async move {
            let call_start = std::time::Instant::now();
            let mut emitter = BedrockStreamEmitter::new(&model, stream_sink);
            let resp = http
                .post_stream(&url, &headers, &body, &mut emitter)
                .await?;

            if resp.status != 200 {
                return Err(handle_error_response(
                    resp.status,
                    &resp.headers,
                    &resp.body,
                ));
            }

            // If the upstream returned JSON instead of an event stream (e.g. an
            // immediate error wrapped in 200), fall back to JSON parsing.
            let is_event_stream = resp
                .headers
                .get("content-type")
                .is_some_and(|ct| ct.contains("application/vnd.amazon.eventstream"));
            let provider_resp = if is_event_stream {
                emitter.finish()
            } else {
                let response_id = resp.headers.get("x-amz-request-id").cloned();
                parse_json_response(&resp.body, &model, response_id)?
            };
            record_telemetry(&provider_resp, call_start, &model);
            Ok(provider_resp)
        };
        Box::pin(fut.instrument(span))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use serde_json::json;
    use simulacra_types::{FinishReason, ProviderStreamEvent, ProviderStreamSink, Role};
    use std::sync::{Arc, Mutex};

    const MODEL: &str = "anthropic.claude-3-5-sonnet-20240620-v1:0";

    struct FakeHttpClient {
        response: Arc<dyn Fn() -> Result<HttpResponse, ProviderError> + Send + Sync>,
    }

    impl FakeHttpClient {
        fn with_response(status: u16, body: &[u8]) -> Self {
            Self::with_response_and_headers(status, body, HashMap::new())
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

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    struct CapturingHttpClient {
        captured: Arc<tokio::sync::Mutex<Vec<CapturedRequest>>>,
        status: u16,
        response_headers: HashMap<String, String>,
        response_body: Vec<u8>,
    }

    impl CapturingHttpClient {
        #[allow(dead_code)]
        fn new(
            status: u16,
            response_body: &[u8],
        ) -> (Self, Arc<tokio::sync::Mutex<Vec<CapturedRequest>>>) {
            Self::new_with_headers(status, response_body, HashMap::new())
        }

        fn new_with_headers(
            status: u16,
            response_body: &[u8],
            response_headers: HashMap<String, String>,
        ) -> (Self, Arc<tokio::sync::Mutex<Vec<CapturedRequest>>>) {
            let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            (
                Self {
                    captured: Arc::clone(&captured),
                    status,
                    response_headers,
                    response_body: response_body.to_vec(),
                },
                captured,
            )
        }
    }

    impl HttpClient for CapturingHttpClient {
        fn post(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &[u8],
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + '_>>
        {
            let captured = Arc::clone(&self.captured);
            let request = CapturedRequest {
                url: url.to_owned(),
                headers: headers.to_vec(),
                body: body.to_vec(),
            };
            let status = self.status;
            let response_headers = self.response_headers.clone();
            let response_body = self.response_body.clone();
            Box::pin(async move {
                captured.lock().await.push(request);
                Ok(HttpResponse {
                    status,
                    headers: response_headers,
                    body: response_body,
                })
            })
        }

        fn post_stream<'a>(
            &'a self,
            url: &'a str,
            headers: &'a [(String, String)],
            body: &'a [u8],
            sink: &'a mut dyn HttpStreamSink,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + 'a>>
        {
            let captured = Arc::clone(&self.captured);
            let request = CapturedRequest {
                url: url.to_owned(),
                headers: headers.to_vec(),
                body: body.to_vec(),
            };
            let status = self.status;
            let response_headers = self.response_headers.clone();
            let response_body = self.response_body.clone();
            Box::pin(async move {
                captured.lock().await.push(request);
                sink.begin(status, &response_headers)?;
                sink.chunk(&response_body)?;
                Ok(HttpResponse {
                    status,
                    headers: response_headers,
                    body: Vec::new(),
                })
            })
        }
    }

    #[derive(Default)]
    struct RecordingProviderStreamSink {
        events: Mutex<Vec<ProviderStreamEvent>>,
    }

    impl RecordingProviderStreamSink {
        fn texts(&self) -> Vec<String> {
            self.events()
                .into_iter()
                .filter_map(|event| match event {
                    ProviderStreamEvent::TextDelta { text } => Some(text),
                    _ => None,
                })
                .collect()
        }

        fn events(&self) -> Vec<ProviderStreamEvent> {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl ProviderStreamSink for RecordingProviderStreamSink {
        fn emit(&self, event: ProviderStreamEvent) {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
        }
    }

    fn provider(http: Box<dyn HttpClient>) -> BedrockProvider {
        BedrockProvider::with_http_client("AKIDEXAMPLE", "secret", "us-east-1", MODEL, http)
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

    fn system(content: &str) -> Message {
        Message {
            role: Role::System,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }
    }

    fn success_response_json() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{ "text": "Hello, world!" }]
                }
            },
            "stopReason": "end_turn",
            "usage": { "inputTokens": 10, "outputTokens": 25 },
            "model": MODEL
        }))
        .unwrap()
    }

    fn tool_use_response_json() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{
                        "toolUse": {
                            "toolUseId": "tu_1",
                            "name": "get_weather",
                            "input": { "location": "SF" }
                        }
                    }]
                }
            },
            "stopReason": "tool_use",
            "usage": { "inputTokens": 10, "outputTokens": 25 },
            "model": MODEL
        }))
        .unwrap()
    }

    fn response_headers() -> HashMap<String, String> {
        let mut headers = HashMap::new();
        headers.insert("x-amz-request-id".to_owned(), "req-bedrock-123".to_owned());
        headers
    }

    fn error_body(message: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({ "message": message })).unwrap()
    }

    fn fresh_budget() -> ResourceBudget {
        ResourceBudget::new(100_000, 100, Decimal::new(100, 0), 10)
    }

    fn exhausted_budget() -> ResourceBudget {
        let mut b = ResourceBudget::new(100, 100, Decimal::new(100, 0), 10);
        b.used_tokens = 100;
        b
    }

    /// Build a single event-stream frame for testing.
    fn frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.push(name.len() as u8);
            header_bytes.extend_from_slice(name.as_bytes());
            header_bytes.push(6); // string value type
            let v = value.as_bytes();
            header_bytes.extend_from_slice(&(v.len() as u16).to_be_bytes());
            header_bytes.extend_from_slice(v);
        }
        let headers_len = header_bytes.len() as u32;
        let total_len = (12 + header_bytes.len() + payload.len() + 4) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&total_len.to_be_bytes());
        out.extend_from_slice(&headers_len.to_be_bytes());
        out.extend_from_slice(&[0u8; 4]); // prelude crc (ignored)
        out.extend_from_slice(&header_bytes);
        out.extend_from_slice(payload);
        out.extend_from_slice(&[0u8; 4]); // message crc (ignored)
        out
    }

    fn event_frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
        frame(
            &[
                (":message-type", "event"),
                (":event-type", event_type),
                (":content-type", "application/json"),
            ],
            payload,
        )
    }

    fn streaming_text_body() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(event_frame("messageStart", br#"{"role":"assistant"}"#));
        body.extend(event_frame(
            "contentBlockDelta",
            br#"{"contentBlockIndex":0,"delta":{"text":"Hel"}}"#,
        ));
        body.extend(event_frame(
            "contentBlockDelta",
            br#"{"contentBlockIndex":0,"delta":{"text":"lo"}}"#,
        ));
        body.extend(event_frame(
            "contentBlockStop",
            br#"{"contentBlockIndex":0}"#,
        ));
        body.extend(event_frame(
            "messageDelta",
            br#"{"delta":{"stopReason":"end_turn"}}"#,
        ));
        body.extend(event_frame(
            "metadata",
            br#"{"usage":{"inputTokens":11,"outputTokens":7}}"#,
        ));
        body.extend(event_frame("messageStop", br#"{"stopReason":"end_turn"}"#));
        body
    }

    fn streaming_tool_body() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(event_frame("messageStart", br#"{"role":"assistant"}"#));
        body.extend(event_frame(
            "contentBlockStart",
            br#"{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"tu_1","name":"get_weather"}}}"#,
        ));
        body.extend(event_frame(
            "contentBlockDelta",
            br#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":"{\"loc"}}}"#,
        ));
        body.extend(event_frame(
            "contentBlockDelta",
            br#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":"ation\":\"SF\"}"}}}"#,
        ));
        body.extend(event_frame(
            "contentBlockStop",
            br#"{"contentBlockIndex":0}"#,
        ));
        body.extend(event_frame(
            "messageDelta",
            br#"{"delta":{"stopReason":"tool_use"}}"#,
        ));
        body.extend(event_frame(
            "metadata",
            br#"{"usage":{"inputTokens":11,"outputTokens":7}}"#,
        ));
        body.extend(event_frame("messageStop", br#"{"stopReason":"tool_use"}"#));
        body
    }

    fn streaming_exception_body(message: &str) -> Vec<u8> {
        frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "throttlingException"),
                (":content-type", "application/json"),
            ],
            &serde_json::to_vec(&json!({ "message": message })).unwrap(),
        )
    }

    #[tokio::test]
    async fn budget_exhausted_returns_error_without_http_call() {
        let fake = FakeHttpClient {
            response: Arc::new(|| panic!("HTTP should not be called when budget is exhausted")),
        };
        let provider = provider(Box::new(fake));
        let messages = vec![user("hello")];
        let mut budget = exhausted_budget();

        let err = provider
            .chat(&messages, &[], &mut budget)
            .await
            .unwrap_err();

        assert!(matches!(err, ProviderError::BudgetExhausted(_)));
    }

    #[tokio::test]
    async fn successful_text_response_maps_content_usage_finish_reason_response_id_model() {
        let fake = FakeHttpClient::with_response_and_headers(
            200,
            &success_response_json(),
            response_headers(),
        );
        let provider = provider(Box::new(fake));
        let messages = vec![user("hello")];
        let mut budget = fresh_budget();

        let resp = provider.chat(&messages, &[], &mut budget).await.unwrap();

        assert_eq!(resp.message.role, Role::Assistant);
        assert_eq!(resp.message.content, "Hello, world!");
        assert!(resp.message.tool_calls.is_empty());
        assert!(resp.message.provider_content.is_empty());
        assert_eq!(resp.token_usage.input_tokens, 10);
        assert_eq!(resp.token_usage.output_tokens, 25);
        assert_eq!(resp.finish_reason, FinishReason::EndTurn);
        assert_eq!(
            resp.provider_response_id.as_deref(),
            Some("req-bedrock-123")
        );
        assert_eq!(resp.model, MODEL);
    }

    #[tokio::test]
    async fn tool_use_response_maps_tool_calls_with_parsed_json() {
        let fake = FakeHttpClient::with_response_and_headers(
            200,
            &tool_use_response_json(),
            response_headers(),
        );
        let provider = provider(Box::new(fake));
        let messages = vec![user("weather")];
        let mut budget = fresh_budget();

        let resp = provider.chat(&messages, &[], &mut budget).await.unwrap();

        assert_eq!(resp.message.tool_calls.len(), 1);
        assert_eq!(resp.message.tool_calls[0].id, "tu_1");
        assert_eq!(resp.message.tool_calls[0].name, "get_weather");
        assert!(resp.message.provider_content.is_empty());
        assert_eq!(
            resp.message.tool_calls[0].arguments,
            json!({"location":"SF"})
        );
        assert_eq!(resp.finish_reason, FinishReason::ToolUse);
    }

    #[tokio::test]
    async fn error_400_maps_to_non_retryable_bad_request() {
        let fake = FakeHttpClient::with_response(400, &error_body("bad request"));
        let provider = provider(Box::new(fake));
        let messages = vec![user("hello")];
        let mut budget = fresh_budget();

        let err = provider
            .chat(&messages, &[], &mut budget)
            .await
            .unwrap_err();

        assert!(!err.is_retryable());
        assert!(matches!(err, ProviderError::BadRequest(_)));
    }

    #[tokio::test]
    async fn error_401_maps_to_non_retryable_auth_error() {
        let fake = FakeHttpClient::with_response(401, &error_body("bad credentials"));
        let provider = provider(Box::new(fake));
        let messages = vec![user("hello")];
        let mut budget = fresh_budget();

        let err = provider
            .chat(&messages, &[], &mut budget)
            .await
            .unwrap_err();

        assert!(!err.is_retryable());
        assert!(matches!(err, ProviderError::AuthError(_)));
    }

    #[tokio::test]
    async fn error_429_maps_to_retryable_rate_limit() {
        let fake = FakeHttpClient::with_response(429, &error_body("too many requests"));
        let provider = provider(Box::new(fake));
        let messages = vec![user("hello")];
        let mut budget = fresh_budget();

        let err = provider
            .chat(&messages, &[], &mut budget)
            .await
            .unwrap_err();

        assert!(err.is_retryable());
        assert!(matches!(err, ProviderError::RateLimit { .. }));
    }

    #[tokio::test]
    async fn error_500_maps_to_retryable_server_error() {
        let fake = FakeHttpClient::with_response(500, &error_body("internal error"));
        let provider = provider(Box::new(fake));
        let messages = vec![user("hello")];
        let mut budget = fresh_budget();

        let err = provider
            .chat(&messages, &[], &mut budget)
            .await
            .unwrap_err();

        assert!(err.is_retryable());
        assert!(matches!(err, ProviderError::ServerError(_)));
    }

    #[tokio::test]
    async fn bedrock_error_message_is_surfaced() {
        let fake = FakeHttpClient::with_response(400, &error_body("bad model id"));
        let provider = provider(Box::new(fake));
        let messages = vec![user("hello")];
        let mut budget = fresh_budget();

        let err = provider
            .chat(&messages, &[], &mut budget)
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("bad model id"),
            "expected Bedrock message in error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn provider_trait_is_object_safe() {
        let fake = FakeHttpClient::with_response_and_headers(
            200,
            &success_response_json(),
            response_headers(),
        );
        let provider: Box<dyn Provider> = Box::new(provider(Box::new(fake)));
        let messages = vec![user("hello")];
        let mut budget = fresh_budget();

        let result = provider.chat(&messages, &[], &mut budget).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn provider_returns_usage_without_mutating_budget() {
        let fake = FakeHttpClient::with_response_and_headers(
            200,
            &success_response_json(),
            response_headers(),
        );
        let provider = provider(Box::new(fake));
        let messages = vec![user("hello")];
        let mut budget = fresh_budget();

        let resp = provider.chat(&messages, &[], &mut budget).await.unwrap();

        assert_eq!(resp.token_usage.total(), 35);
        assert_eq!(budget.used_tokens, 0);
    }

    #[tokio::test]
    async fn request_body_shape() {
        let (capturing, captured) = CapturingHttpClient::new_with_headers(
            200,
            &success_response_json(),
            response_headers(),
        );
        let provider = provider(Box::new(capturing));
        let messages = vec![system("be brief"), user("hello")];
        let tools = vec![ToolDefinition {
            name: "get_weather".into(),
            description: "Get weather for a location".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "location": { "type": "string" }
                },
                "required": ["location"]
            }),
        }];
        let mut budget = fresh_budget();

        provider.chat(&messages, &tools, &mut budget).await.unwrap();

        let requests = captured.lock().await;
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["modelId"], MODEL);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["inferenceConfig"]["maxTokens"], 8192);
        assert_eq!(body["system"][0]["text"], "be brief");
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["name"],
            "get_weather"
        );
    }

    #[tokio::test]
    async fn model_id_is_url_path_encoded_in_request_target() {
        let (capturing, captured) = CapturingHttpClient::new_with_headers(
            200,
            &success_response_json(),
            response_headers(),
        );
        let provider = provider(Box::new(capturing));
        let messages = vec![user("hello")];
        let mut budget = fresh_budget();

        provider.chat(&messages, &[], &mut budget).await.unwrap();

        let requests = captured.lock().await;
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .url
                .contains("/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/")
        );
        assert!(requests[0].url.ends_with("/converse"));
    }

    #[tokio::test]
    async fn sigv4_headers_present() {
        let (capturing, captured) = CapturingHttpClient::new_with_headers(
            200,
            &success_response_json(),
            response_headers(),
        );
        let provider = provider(Box::new(capturing));
        let messages = vec![user("hello")];
        let mut budget = fresh_budget();

        provider.chat(&messages, &[], &mut budget).await.unwrap();

        let requests = captured.lock().await;
        assert_eq!(requests.len(), 1);
        let headers: HashMap<String, String> = requests[0]
            .headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        assert!(
            headers
                .get("authorization")
                .is_some_and(|v| v.starts_with("AWS4-HMAC-SHA256"))
        );
        assert!(headers.contains_key("x-amz-date"));
        assert!(headers.contains_key("x-amz-content-sha256"));
    }

    #[tokio::test]
    async fn chat_stream_emits_text_deltas_and_assembles_final_response() {
        let mut headers = response_headers();
        headers.insert(
            "content-type".to_owned(),
            "application/vnd.amazon.eventstream".to_owned(),
        );
        let fake = FakeHttpClient::with_response_and_headers(200, &streaming_text_body(), headers);
        let provider = provider(Box::new(fake));
        let messages = vec![user("say hello")];
        let mut budget = fresh_budget();
        let sink = RecordingProviderStreamSink::default();

        let resp = StreamingProvider::chat_stream(&provider, &messages, &[], &mut budget, &sink)
            .await
            .unwrap();

        assert_eq!(sink.texts(), vec!["Hel", "lo"]);
        assert_eq!(resp.message.role, Role::Assistant);
        assert_eq!(resp.message.content, "Hello");
        assert!(resp.message.tool_calls.is_empty());
        assert!(resp.message.provider_content.is_empty());
        assert_eq!(resp.finish_reason, FinishReason::EndTurn);
        assert_eq!(resp.token_usage.input_tokens, 11);
        assert_eq!(resp.token_usage.output_tokens, 7);
    }

    #[tokio::test]
    async fn chat_stream_propagates_encoded_exception_frame_message_as_error() {
        let mut headers = response_headers();
        headers.insert(
            "content-type".to_owned(),
            "application/vnd.amazon.eventstream".to_owned(),
        );
        let message = "Bedrock throttled the model stream";
        let fake = FakeHttpClient::with_response_and_headers(
            200,
            &streaming_exception_body(message),
            headers,
        );
        let provider = provider(Box::new(fake));
        let mut budget = fresh_budget();
        let sink = RecordingProviderStreamSink::default();

        let error = StreamingProvider::chat_stream(
            &provider,
            &[user("stream this")],
            &[],
            &mut budget,
            &sink,
        )
        .await
        .expect_err("encoded exception frame must not return partial or empty success");

        assert!(
            error.to_string().contains(message),
            "streaming error lost Bedrock payload message: {error}"
        );
        assert!(sink.events().is_empty());
    }

    #[tokio::test]
    async fn chat_stream_emits_tool_call_deltas_and_assembles_final_response() {
        let mut headers = response_headers();
        headers.insert(
            "content-type".to_owned(),
            "application/vnd.amazon.eventstream".to_owned(),
        );
        let fake = FakeHttpClient::with_response_and_headers(200, &streaming_tool_body(), headers);
        let provider = provider(Box::new(fake));
        let messages = vec![user("weather")];
        let mut budget = fresh_budget();
        let sink = RecordingProviderStreamSink::default();

        let resp = StreamingProvider::chat_stream(&provider, &messages, &[], &mut budget, &sink)
            .await
            .unwrap();

        let tool_deltas: Vec<_> = sink
            .events()
            .into_iter()
            .filter(|event| matches!(event, ProviderStreamEvent::ToolCallDelta { .. }))
            .collect();
        assert_eq!(
            tool_deltas,
            vec![
                ProviderStreamEvent::ToolCallDelta {
                    index: 0,
                    tool_call_id: Some("tu_1".into()),
                    name: Some("get_weather".into()),
                    arguments_delta: String::new(),
                },
                ProviderStreamEvent::ToolCallDelta {
                    index: 0,
                    tool_call_id: Some("tu_1".into()),
                    name: Some("get_weather".into()),
                    arguments_delta: "{\"loc".into(),
                },
                ProviderStreamEvent::ToolCallDelta {
                    index: 0,
                    tool_call_id: Some("tu_1".into()),
                    name: Some("get_weather".into()),
                    arguments_delta: "ation\":\"SF\"}".into(),
                },
            ]
        );
        assert!(resp.message.content.is_empty());
        assert_eq!(resp.message.tool_calls.len(), 1);
        assert_eq!(resp.message.tool_calls[0].id, "tu_1");
        assert_eq!(resp.message.tool_calls[0].name, "get_weather");
        assert_eq!(
            resp.message.tool_calls[0].arguments,
            json!({"location":"SF"})
        );
        assert!(resp.message.provider_content.is_empty());
        assert_eq!(resp.finish_reason, FinishReason::ToolUse);
    }
}
