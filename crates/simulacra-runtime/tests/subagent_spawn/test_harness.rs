
use rust_decimal::Decimal;
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, ProjectConfig, SimulacraConfig, TierMap,
    VfsConfig,
};
use simulacra_runtime::{
    AgentLoop, AgentLoopConfig, AgentLoopOutput, AgentSupervisor, AgentTaskFactory,
    BoxTaskFuture, CancellationToken, ChildAgentStatus, ChildStatusTool, ChildTerminalResult,
    CloseChildAgentTool, DEFAULT_SYSTEM_PROMPT, InMemoryJournalStorage, JoinChildAgentTool,
    ListChildAgentTool, MessagePriority, NoopActivitySink, ProviderKind, RestartStrategy,
    RuntimeError, SpawnAck, SpawnAgentTool, SpawnConfig, SteerChildAgentTool, SupervisorMessage,
    SupervisorPayload, TaskFactory, TurnResult, WaitChildAgentTool,
};
use simulacra_tool::ToolRegistry;
use simulacra_types::{
    AgentId, CapabilityToken, ContextStrategy, ExitReason, FinishReason, JournalEntry,
    JournalEntryKind, JournalStorage, Message, NetworkPermission, PathPattern, Provider,
    ProviderError, ProviderResponse, ResourceBudget, Role, TokenUsage, Tool, ToolCallMessage,
    ToolDefinition, ToolError, VirtualFs,
};
use simulacra_vfs::MemoryFs;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::sync::Notify;
use tracing_subscriber::layer::SubscriberExt;

static OPENAI_ENV_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn openai_env_guard() -> std::sync::MutexGuard<'static, ()> {
    OPENAI_ENV_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

#[derive(Debug, Clone)]
struct CannedResponse {
    status: u16,
    body: Vec<u8>,
}

impl CannedResponse {
    fn json(body: serde_json::Value) -> Self {
        Self {
            status: 200,
            body: serde_json::to_vec(&body).expect("response JSON should serialize"),
        }
    }
}

struct FakeOpenAiServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FakeOpenAiServer {
    fn new(response: CannedResponse) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake upstream should bind");
        listener
            .set_nonblocking(true)
            .expect("fake upstream should become nonblocking");
        let addr = listener
            .local_addr()
            .expect("listener should have a local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let requests_for_thread = Arc::clone(&requests);
        let shutdown_for_thread = Arc::clone(&shutdown);
        let response = Arc::new(response);

        let handle = thread::spawn(move || {
            while !shutdown_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _peer)) => {
                        if shutdown_for_thread.load(Ordering::SeqCst) {
                            break;
                        }
                        stream
                            .set_nonblocking(false)
                            .expect("accepted fake upstream connection should be blocking");

                        let request = read_http_request(&mut stream)
                            .expect("fake upstream should read a complete HTTP request");
                        requests_for_thread
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(request);
                        write_http_response(&mut stream, &response)
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

    fn first_request_json(&self) -> serde_json::Value {
        let body = self
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .first()
            .cloned()
            .expect("expected at least one captured request")
            .body;
        serde_json::from_slice(&body).expect("captured request body should be valid JSON")
    }
}

impl Drop for FakeOpenAiServer {
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

    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| {
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let mut chunk = vec![0_u8; content_length - body.len()];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Ok(CapturedRequest { method, path, body })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn write_http_response(stream: &mut TcpStream, response: &CannedResponse) -> std::io::Result<()> {
    let status_text = if response.status == 200 {
        "OK"
    } else {
        "ERROR"
    };
    let headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        status_text,
        response.body.len()
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    parent_name: Option<String>,
    fields: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct CapturedEvent {
    level: String,
    _current_span: Option<String>,
    fields: HashMap<String, String>,
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);
        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            parent_name: ctx.lookup_current().map(|span| span.name().to_string()),
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
            let mut new_fields = HashMap::new();
            let mut visitor = FieldVisitor(&mut new_fields);
            values.record(&mut visitor);

            let mut spans = self.spans.lock().unwrap();
            for captured in spans.iter_mut().rev() {
                if captured.name == span_name {
                    for (key, value) in new_fields {
                        captured.fields.insert(key, value);
                    }
                    break;
                }
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        event.record(&mut visitor);
        self.events.lock().unwrap().push(CapturedEvent {
            level: event.metadata().level().to_string(),
            _current_span: ctx.lookup_current().map(|span| span.name().to_string()),
            fields,
        });
    }
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

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

fn capture_trace<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    static TRACING_CAPTURE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _capture_guard = TRACING_CAPTURE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    });
    let result = tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        let result = f();
        tracing::callsite::rebuild_interest_cache();
        result
    });
    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

#[derive(Debug)]
struct FakeProvider {
    responses: Mutex<Vec<ProviderResponse>>,
    calls: AtomicUsize,
}

impl FakeProvider {
    fn new(responses: Vec<ProviderResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            calls: AtomicUsize::new(0),
        }
    }
}

impl Provider for FakeProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut responses = self.responses.lock().unwrap();
            Ok(responses
                .pop()
                .expect("fake provider should have a canned response"))
        })
    }
}

struct ExtraProbeTool;

impl Tool for ExtraProbeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "child_extra_probe".to_string(),
            description: "Extra child tool registered by the embedding runtime.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        _arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>,
    > {
        Box::pin(async { Ok(serde_json::json!({"ok": true})) })
    }
}

struct PassthroughContext;

impl ContextStrategy for PassthroughContext {
    fn compact(&self, messages: &[Message], _token_limit: u64) -> Vec<Message> {
        messages.to_vec()
    }
}
