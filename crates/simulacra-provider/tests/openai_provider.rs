// crates/simulacra-provider/tests/s007_openai_red.rs

#![allow(clippy::type_complexity, clippy::await_holding_lock)]

use rust_decimal::Decimal;
use serde_json::json;
use simulacra_provider::{
    FinishReason, Message, OpenAiProvider, Provider, ProviderError, ResourceBudget,
};
use simulacra_types::{Role, ToolDefinition};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug, Clone)]
struct CannedResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CannedResponse {
    fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_vec(&body).expect("response JSON should serialize"),
        }
    }

    fn sse(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body,
        }
    }
}

struct FakeHttpClient {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FakeHttpClient {
    fn new(response: CannedResponse) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake upstream should bind");
        listener
            .set_nonblocking(true)
            .expect("fake upstream should become nonblocking");
        let addr = listener
            .local_addr()
            .expect("listener should have a local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let response = Arc::new(response);

        let requests_for_thread = Arc::clone(&requests);
        let shutdown_for_thread = Arc::clone(&shutdown);
        let response_for_thread = Arc::clone(&response);
        let handle = thread::spawn(move || {
            while !shutdown_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _peer)) => {
                        if shutdown_for_thread.load(Ordering::SeqCst) {
                            break;
                        }

                        let request = read_http_request(&mut stream)
                            .expect("fake upstream should read a complete HTTP request");
                        requests_for_thread
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(request);
                        write_http_response(&mut stream, &response_for_thread)
                            .expect("fake upstream should write a response");
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("fake upstream accept failed: {err}"),
                }
            }
        });

        Self {
            addr,
            requests,
            shutdown,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn request_count(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    fn first_request(&self) -> CapturedRequest {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .first()
            .cloned()
            .expect("expected at least one captured request")
    }
}

impl Drop for FakeHttpClient {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .expect("fake upstream thread should join cleanly");
        }
    }
}

struct EnvGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

struct OpenAiEndpointGuards {
    _base_url: EnvGuard,
    _api_base: EnvGuard,
}

impl OpenAiEndpointGuards {
    fn set(base_url: &str) -> Self {
        Self {
            _base_url: EnvGuard::set("OPENAI_BASE_URL", base_url),
            _api_base: EnvGuard::set("OPENAI_API_BASE", base_url),
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<CapturedRequest> {
    let mut buffer = Vec::new();
    let mut header_end = None;

    while header_end.is_none() {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        header_end = find_header_end(&buffer);
    }

    let header_end = header_end.expect("HTTP request should include header terminator");
    let header_bytes = &buffer[..header_end];
    let header_text =
        std::str::from_utf8(header_bytes).expect("HTTP request headers should be valid UTF-8");
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .expect("HTTP request should contain a request line");
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .expect("request line should include a method")
        .to_string();
    let path = request_parts
        .next()
        .expect("request line should include a path")
        .to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .expect("header lines should contain a colon");
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    let mut body = buffer[body_start..].to_vec();
    while body.len() < content_length {
        let mut chunk = vec![0_u8; content_length - body.len()];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Ok(CapturedRequest {
        method,
        path,
        headers,
        body,
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn write_http_response(stream: &mut TcpStream, response: &CannedResponse) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        format!(
            "HTTP/1.1 {} {}\r\n",
            response.status,
            status_text(response.status)
        )
        .as_bytes(),
    );

    let mut has_length = false;
    for (name, value) in &response.headers {
        if name.eq_ignore_ascii_case("content-length") {
            has_length = true;
        }
        bytes.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    if !has_length {
        bytes.extend_from_slice(format!("content-length: {}\r\n", response.body.len()).as_bytes());
    }
    bytes.extend_from_slice(b"connection: close\r\n\r\n");
    bytes.extend_from_slice(&response.body);
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

fn fresh_budget() -> ResourceBudget {
    ResourceBudget::new(100_000, 100, Decimal::new(100, 0), 10)
}

fn exhausted_budget() -> ResourceBudget {
    let mut budget = ResourceBudget::new(100, 100, Decimal::new(100, 0), 10);
    budget.used_tokens = 100;
    budget
}

fn user_message(content: &str) -> Message {
    Message {
        role: Role::User,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
    }
}

fn weather_tool() -> ToolDefinition {
    ToolDefinition {
        name: "get_weather".into(),
        description: "Get weather for a location".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "location": { "type": "string" }
            },
            "required": ["location"]
        }),
    }
}

fn success_response_json() -> serde_json::Value {
    json!({
        "id": "chatcmpl_test123",
        "object": "chat.completion",
        "created": 1_726_000_000_u64,
        "model": "gpt-4o-mini",
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello, world!"
                },
                "finish_reason": "stop"
            }
        ],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 25,
            "total_tokens": 35
        }
    })
}

fn tool_call_response_json() -> serde_json::Value {
    json!({
        "id": "chatcmpl_tool456",
        "object": "chat.completion",
        "created": 1_726_000_001_u64,
        "model": "gpt-4o-mini",
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_abc123",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"location\":\"San Francisco\"}"
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }
        ],
        "usage": {
            "prompt_tokens": 50,
            "completion_tokens": 100,
            "total_tokens": 150
        }
    })
}

fn streaming_response_body() -> Vec<u8> {
    concat!(
        "data: {\"id\":\"chatcmpl_stream789\",\"object\":\"chat.completion.chunk\",\"created\":1726000002,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl_stream789\",\"object\":\"chat.completion.chunk\",\"created\":1726000002,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\", stream!\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl_stream789\",\"object\":\"chat.completion.chunk\",\"created\":1726000002,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"chatcmpl_stream789\",\"object\":\"chat.completion.chunk\",\"created\":1726000002,\"model\":\"gpt-4o-mini\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}\n\n",
        "data: [DONE]\n\n"
    )
    .as_bytes()
    .to_vec()
}

#[tokio::test(flavor = "current_thread")]
async fn budget_exhausted_returns_error_without_http_call() {
    let _test_guard = test_guard();
    let fake = FakeHttpClient::new(CannedResponse::json(200, success_response_json()));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let messages = vec![user_message("Hello")];
    let mut budget = exhausted_budget();

    let result = provider.chat(&messages, &[], &mut budget).await;

    assert!(
        matches!(result, Err(ProviderError::BudgetExhausted(_))),
        "expected BudgetExhausted, got: {result:?}"
    );
    assert_eq!(
        fake.request_count(),
        0,
        "budget must be checked before any HTTP request is made"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn successful_text_response_maps_correctly() {
    let _test_guard = test_guard();
    let fake = FakeHttpClient::new(CannedResponse::json(200, success_response_json()));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let messages = vec![user_message("Hello")];
    let mut budget = fresh_budget();

    let resp = provider
        .chat(&messages, &[], &mut budget)
        .await
        .expect("OpenAI text responses should map into ProviderResponse");

    assert_eq!(resp.message.role, Role::Assistant);
    assert_eq!(resp.message.content, "Hello, world!");
    assert!(resp.message.tool_calls.is_empty());
    assert_eq!(resp.token_usage.input_tokens, 10);
    assert_eq!(resp.token_usage.output_tokens, 25);
    assert_eq!(resp.finish_reason, FinishReason::EndTurn);
    assert_eq!(
        resp.provider_response_id,
        Some("chatcmpl_test123".to_string())
    );
    assert_eq!(resp.model, "gpt-4o-mini");

    let request = fake.first_request();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer test-key")
    );
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("request body should be valid JSON");
    assert_eq!(body["model"], "gpt-4o-mini");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"], "Hello");
}

#[tokio::test(flavor = "current_thread")]
async fn tool_call_response_maps_correctly() {
    let _test_guard = test_guard();
    let fake = FakeHttpClient::new(CannedResponse::json(200, tool_call_response_json()));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let messages = vec![user_message("What's the weather in San Francisco?")];
    let tools = vec![weather_tool()];
    let mut budget = fresh_budget();

    let resp = provider
        .chat(&messages, &tools, &mut budget)
        .await
        .expect("OpenAI tool-call responses should map into ProviderResponse");

    assert_eq!(resp.message.role, Role::Assistant);
    assert_eq!(resp.message.content, "");
    assert_eq!(resp.message.tool_calls.len(), 1);
    assert_eq!(resp.message.tool_calls[0].id, "call_abc123");
    assert_eq!(resp.message.tool_calls[0].name, "get_weather");
    assert_eq!(
        resp.message.tool_calls[0].arguments,
        json!({"location": "San Francisco"})
    );
    assert_eq!(resp.finish_reason, FinishReason::ToolUse);
    assert_eq!(resp.token_usage.input_tokens, 50);
    assert_eq!(resp.token_usage.output_tokens, 100);
}

#[tokio::test(flavor = "current_thread")]
async fn rate_limit_429_is_retryable() {
    let _test_guard = test_guard();
    let fake = FakeHttpClient::new(CannedResponse {
        status: 429,
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("retry-after".into(), "30".into()),
        ],
        body: serde_json::to_vec(&json!({
            "error": {
                "message": "too many requests",
                "type": "rate_limit_error"
            }
        }))
        .expect("error response should serialize"),
    });
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let mut budget = fresh_budget();
    let err = provider
        .chat(&[user_message("Hello")], &[], &mut budget)
        .await
        .expect_err("429 responses should return a typed provider error");

    assert!(err.is_retryable(), "429 responses must be retryable");
    match err {
        ProviderError::RateLimit { retry_after_ms } => {
            assert_eq!(retry_after_ms, Some(30_000));
        }
        other => panic!("expected RateLimit error, got: {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn auth_error_401_is_not_retryable() {
    let _test_guard = test_guard();
    let fake = FakeHttpClient::new(CannedResponse::json(
        401,
        json!({
            "error": {
                "message": "invalid api key",
                "type": "invalid_request_error"
            }
        }),
    ));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let mut budget = fresh_budget();
    let err = provider
        .chat(&[user_message("Hello")], &[], &mut budget)
        .await
        .expect_err("401 responses should return a typed provider error");

    assert!(
        !err.is_retryable(),
        "401 auth failures must not be retryable"
    );
    assert!(matches!(err, ProviderError::AuthError(_)));
}

#[tokio::test(flavor = "current_thread")]
async fn bad_request_400_is_not_retryable() {
    let _test_guard = test_guard();
    let fake = FakeHttpClient::new(CannedResponse::json(
        400,
        json!({
            "error": {
                "message": "tools[0].function.name is required",
                "type": "invalid_request_error"
            }
        }),
    ));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let mut budget = fresh_budget();
    let err = provider
        .chat(&[user_message("Hello")], &[], &mut budget)
        .await
        .expect_err("400 responses should return a typed provider error");

    assert!(
        !err.is_retryable(),
        "400 request validation failures must not be retryable"
    );
    assert!(matches!(err, ProviderError::BadRequest(_)));
}

#[tokio::test(flavor = "current_thread")]
async fn provider_trait_is_object_safe() {
    let _test_guard = test_guard();
    let fake = FakeHttpClient::new(CannedResponse::json(200, success_response_json()));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider: Box<dyn Provider> = Box::new(OpenAiProvider::new("test-key", "gpt-4o-mini"));

    let messages = vec![user_message("Hello")];
    let mut budget = fresh_budget();

    let result = provider.chat(&messages, &[], &mut budget).await;
    assert!(result.is_ok(), "Box<dyn Provider> should be callable");
}

#[tokio::test(flavor = "current_thread")]
async fn provider_returns_usage_without_mutating_budget() {
    let _test_guard = test_guard();
    let fake = FakeHttpClient::new(CannedResponse::json(200, success_response_json()));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let mut budget = fresh_budget();
    assert_eq!(budget.used_tokens, 0);

    let resp = provider
        .chat(&[user_message("Hello")], &[], &mut budget)
        .await
        .expect("successful responses should include token usage");

    assert_eq!(resp.token_usage.total(), 35);
    assert_eq!(
        budget.used_tokens, 0,
        "providers must return usage without mutating caller-owned budget accounting"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn streaming_event_stream_is_assembled_into_final_provider_response() {
    let _test_guard = test_guard();
    let fake = FakeHttpClient::new(CannedResponse::sse(streaming_response_body()));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let mut budget = fresh_budget();
    let resp = provider
        .chat(&[user_message("Say hello")], &[], &mut budget)
        .await
        .expect("SSE responses should assemble into a final ProviderResponse");

    assert_eq!(resp.message.role, Role::Assistant);
    assert_eq!(resp.message.content, "Hello, stream!");
    assert!(resp.message.tool_calls.is_empty());
    assert_eq!(resp.token_usage.input_tokens, 11);
    assert_eq!(resp.token_usage.output_tokens, 7);
    assert_eq!(resp.finish_reason, FinishReason::EndTurn);
    assert_eq!(
        resp.provider_response_id,
        Some("chatcmpl_stream789".to_string())
    );
    assert_eq!(resp.model, "gpt-4o-mini");

    let request = fake.first_request();
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("request body should be valid JSON");
    assert_eq!(body["stream"], true);
}

// ── P5/GP3: Streaming tool-call assembly ──────────────────────────

fn streaming_tool_call_response_body() -> Vec<u8> {
    concat!(
        "data: {\"id\":\"chatcmpl_stream_tool\",\"object\":\"chat.completion.chunk\",\"created\":1726000003,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_tc1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl_stream_tool\",\"object\":\"chat.completion.chunk\",\"created\":1726000003,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"loc\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl_stream_tool\",\"object\":\"chat.completion.chunk\",\"created\":1726000003,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ation\\\":\\\"SF\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl_stream_tool\",\"object\":\"chat.completion.chunk\",\"created\":1726000003,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"id\":\"chatcmpl_stream_tool\",\"object\":\"chat.completion.chunk\",\"created\":1726000003,\"model\":\"gpt-4o-mini\",\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":15,\"total_tokens\":35}}\n\n",
        "data: [DONE]\n\n"
    )
    .as_bytes()
    .to_vec()
}

#[tokio::test(flavor = "current_thread")]
async fn streaming_tool_calls_are_assembled_from_sse_chunks() {
    let _test_guard = test_guard();
    let fake = FakeHttpClient::new(CannedResponse::sse(streaming_tool_call_response_body()));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let mut budget = fresh_budget();
    let resp = provider
        .chat(
            &[user_message("What's the weather?")],
            &[weather_tool()],
            &mut budget,
        )
        .await
        .expect("SSE responses with tool_calls should assemble into ProviderResponse");

    assert_eq!(resp.message.role, Role::Assistant);
    assert_eq!(
        resp.message.tool_calls.len(),
        1,
        "streaming tool_calls must be assembled from SSE delta chunks"
    );
    assert_eq!(resp.message.tool_calls[0].id, "call_tc1");
    assert_eq!(resp.message.tool_calls[0].name, "get_weather");
    assert_eq!(
        resp.message.tool_calls[0].arguments,
        json!({"location": "SF"}),
        "tool call arguments streamed across chunks must be concatenated and parsed"
    );
    assert_eq!(resp.finish_reason, FinishReason::ToolUse);
    assert_eq!(resp.token_usage.input_tokens, 20);
    assert_eq!(resp.token_usage.output_tokens, 15);
    assert_eq!(
        resp.provider_response_id,
        Some("chatcmpl_stream_tool".to_string())
    );
}

// ── P6: Parser failure paths ──────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn empty_choices_array_returns_error() {
    let _test_guard = test_guard();
    let body = json!({
        "id": "chatcmpl_empty",
        "object": "chat.completion",
        "created": 1_726_000_000_u64,
        "model": "gpt-4o-mini",
        "choices": [],
        "usage": {
            "prompt_tokens": 5,
            "completion_tokens": 0,
            "total_tokens": 5
        }
    });
    let fake = FakeHttpClient::new(CannedResponse::json(200, body));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let mut budget = fresh_budget();
    let result = provider
        .chat(&[user_message("Hello")], &[], &mut budget)
        .await;

    assert!(
        result.is_err(),
        "an empty choices array must produce an error, not silently succeed"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn missing_delta_in_sse_chunk_does_not_panic() {
    let _test_guard = test_guard();
    // SSE chunk with a choice that has no delta field at all
    let sse_body = concat!(
        "data: {\"id\":\"chatcmpl_nodelta\",\"object\":\"chat.completion.chunk\",\"created\":1726000004,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"chatcmpl_nodelta\",\"object\":\"chat.completion.chunk\",\"created\":1726000004,\"model\":\"gpt-4o-mini\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":0,\"total_tokens\":5}}\n\n",
        "data: [DONE]\n\n"
    )
    .as_bytes()
    .to_vec();
    let fake = FakeHttpClient::new(CannedResponse::sse(sse_body));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let mut budget = fresh_budget();
    // Must not panic; either an error or an empty-content response is acceptable
    let result = provider
        .chat(&[user_message("Hello")], &[], &mut budget)
        .await;

    // The key assertion: the provider handles missing delta gracefully (no panic).
    // If it succeeds, content should be empty since no delta provided content.
    if let Ok(resp) = result {
        assert_eq!(
            resp.message.content, "",
            "a missing delta should produce empty content, not garbage"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_json_in_sse_data_returns_error() {
    let _test_guard = test_guard();
    let sse_body = concat!(
        "data: {\"id\":\"chatcmpl_ok\",\"object\":\"chat.completion.chunk\",\"created\":1726000005,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
        "data: {NOT VALID JSON AT ALL\n\n",
        "data: [DONE]\n\n"
    )
    .as_bytes()
    .to_vec();
    let fake = FakeHttpClient::new(CannedResponse::sse(sse_body));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let mut budget = fresh_budget();
    let result = provider
        .chat(&[user_message("Hello")], &[], &mut budget)
        .await;

    assert!(
        result.is_err(),
        "malformed JSON in an SSE data line must produce an error"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unexpected_finish_reason_defaults_to_end_turn() {
    let _test_guard = test_guard();
    let body = json!({
        "id": "chatcmpl_unknown_fr",
        "object": "chat.completion",
        "created": 1_726_000_000_u64,
        "model": "gpt-4o-mini",
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Done."
                },
                "finish_reason": "some_future_reason"
            }
        ],
        "usage": {
            "prompt_tokens": 5,
            "completion_tokens": 3,
            "total_tokens": 8
        }
    });
    let fake = FakeHttpClient::new(CannedResponse::json(200, body));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let mut budget = fresh_budget();
    let resp = provider
        .chat(&[user_message("Hello")], &[], &mut budget)
        .await
        .expect("unknown finish_reason should not cause an error");

    assert_eq!(
        resp.finish_reason,
        FinishReason::EndTurn,
        "unrecognized finish_reason values must default to EndTurn"
    );
}

// ── P7/GP2: 500 Internal Server Error ─────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn server_error_500_maps_to_retryable_server_error() {
    let _test_guard = test_guard();
    let fake = FakeHttpClient::new(CannedResponse::json(
        500,
        json!({
            "error": {
                "message": "internal server error",
                "type": "server_error"
            }
        }),
    ));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let mut budget = fresh_budget();
    let err = provider
        .chat(&[user_message("Hello")], &[], &mut budget)
        .await
        .expect_err("500 responses should return a typed provider error");

    assert!(
        err.is_retryable(),
        "500 Internal Server Error must be retryable"
    );
    assert!(
        matches!(err, ProviderError::ServerError(_)),
        "500 must map to ServerError, got: {err:?}"
    );
}

// ── GP1: Retry behavior documentation ──────────────────────────────
// The OpenAI provider does NOT implement retry logic at this layer.
// It classifies errors as retryable/non-retryable via `is_retryable()`,
// but actual retry orchestration is the responsibility of the caller
// (e.g., the agent loop). This is by design — the provider is a
// single-shot HTTP client that maps responses to typed results.
//
// Evidence: `OpenAiProvider::chat()` makes exactly ONE HTTP call per
// invocation. There is no loop, no backoff, no retry counter.
// The `is_retryable()` method on `ProviderError` is a signal to the
// caller, not an internal mechanism.
//
// The test below verifies that a retryable error does NOT trigger
// an automatic retry at the provider level.

#[tokio::test(flavor = "current_thread")]
async fn retryable_error_does_not_trigger_automatic_retry() {
    let _test_guard = test_guard();
    let fake = FakeHttpClient::new(CannedResponse::json(
        500,
        json!({
            "error": {
                "message": "transient failure",
                "type": "server_error"
            }
        }),
    ));
    let _env = OpenAiEndpointGuards::set(&fake.base_url());
    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini");

    let mut budget = fresh_budget();
    let err = provider
        .chat(&[user_message("Hello")], &[], &mut budget)
        .await
        .expect_err("500 should return an error");

    assert!(err.is_retryable(), "500 must be retryable");
    assert_eq!(
        fake.request_count(),
        1,
        "provider must make exactly one HTTP request — retry is the caller's responsibility"
    );
}
