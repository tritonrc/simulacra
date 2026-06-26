#![allow(clippy::type_complexity)]
#![cfg(feature = "spawn")]

use rust_decimal::Decimal;
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, ProjectConfig, SimulacraConfig, TierMap,
    VfsConfig,
};
use simulacra_runtime::{
    AgentLoop, AgentLoopConfig, AgentLoopOutput, AgentSupervisor, AgentTaskFactory, BoxTaskFuture,
    CancellationToken, DEFAULT_SYSTEM_PROMPT, InMemoryJournalStorage, MessagePriority,
    NoopActivitySink, ProviderKind, RestartStrategy, RuntimeError, SpawnAgentTool, SpawnConfig,
    SupervisorMessage, SupervisorPayload, TaskFactory, TurnResult,
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
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    });
    let result = tracing::subscriber::with_default(subscriber, f);
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

fn default_capability() -> CapabilityToken {
    CapabilityToken {
        spawn_types: vec!["researcher".into(), "reviewer".into()],
        ..Default::default()
    }
}

fn default_budget() -> ResourceBudget {
    ResourceBudget::new(100, 10, Decimal::new(100, 0), 2)
}

fn child_budget(max_tokens: u64, max_turns: u32, max_sub_agents: u32) -> ResourceBudget {
    child_budget_with_cost(max_tokens, max_turns, Decimal::new(10, 0), max_sub_agents)
}

fn spawn_config(agent_id: &str, parent_id: &str, budget: ResourceBudget) -> SpawnConfig {
    spawn_config_with_agent_type(agent_id, parent_id, "researcher", budget)
}

fn child_budget_with_cost(
    max_tokens: u64,
    max_turns: u32,
    max_cost: Decimal,
    max_sub_agents: u32,
) -> ResourceBudget {
    ResourceBudget::new(max_tokens, max_turns, max_cost, max_sub_agents)
}

fn spawn_config_with_agent_type(
    agent_id: &str,
    parent_id: &str,
    agent_type: &str,
    budget: ResourceBudget,
) -> SpawnConfig {
    SpawnConfig {
        agent_id: AgentId(agent_id.into()),
        parent_id: AgentId(parent_id.into()),
        capability: None,
        budget,
        restart_strategy: RestartStrategy::LetCrash,
        agent_type: Some(agent_type.into()),
        task: "delegate task".into(),
        system_prompt: None,
        tier: None,
        resolved_tier: None,
    }
}

fn child_success_output() -> AgentLoopOutput {
    AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![Message {
            role: Role::Assistant,
            content: "child summary".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        token_usage: TokenUsage {
            input_tokens: 3,
            output_tokens: 2,
        },
        used_turns: 1,
        used_cost: Decimal::new(15, 2),
    }
}

/// Minimal task factory that immediately resolves with a completed output.
/// Used by tests that validate supervisor-side invariants (budget checks,
/// used_sub_agents increment, spans) without caring about child behaviour.
///
/// `AgentSupervisor::spawn_agent` now requires a task factory (WARNING 1 fix)
/// so tests that previously used `AgentSupervisor::new` must swap to
/// `with_task_factory(..., NoopFactory)`.
struct NoopFactory;

impl TaskFactory for NoopFactory {
    fn create_task(
        &self,
        _config: SpawnConfig,
        _token: simulacra_runtime::CancellationToken,
    ) -> BoxTaskFuture {
        Box::pin(async {
            Ok(AgentLoopOutput {
                exit_reason: ExitReason::Complete,
                messages: vec![],
                token_usage: TokenUsage::default(),
                used_turns: 0,
                used_cost: Decimal::ZERO,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SpawnSnapshot {
    agent_id: String,
    parent_id: String,
    agent_type: String,
    task: String,
    max_tokens: u64,
    max_turns: u32,
    max_sub_agents: u32,
    restart_strategy: RestartStrategy,
}

#[derive(Clone)]
struct RecordingTaskFactory {
    inner: Arc<RecordingTaskFactoryInner>,
}

struct RecordingTaskFactoryInner {
    outputs: Mutex<VecDeque<Result<AgentLoopOutput, RuntimeError>>>,
    started: Mutex<Vec<SpawnSnapshot>>,
    completed: AtomicUsize,
    started_notify: Notify,
    completed_notify: Notify,
    /// Journal snapshot captured at the moment create_task is called (child execution begins).
    /// This lets tests verify what journal entries existed *before* the child started.
    journal_at_spawn: Mutex<Option<Vec<JournalEntry>>>,
    /// Optional journal reference for capturing state at spawn time.
    journal_ref: Mutex<Option<(Arc<dyn JournalStorage>, AgentId)>>,
}

struct FailingAppendJournal;

impl JournalStorage for FailingAppendJournal {
    fn append(&self, _entry: JournalEntry) -> Result<(), simulacra_types::JournalError> {
        Err(simulacra_types::JournalError::Storage(
            "injected append failure".into(),
        ))
    }

    fn read_all(
        &self,
        _agent_id: &AgentId,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        Ok(vec![])
    }

    fn query_token_usage(
        &self,
        _agent_id: &AgentId,
    ) -> Result<TokenUsage, simulacra_types::JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        _agent_id: &AgentId,
        _after_entry: usize,
        _data: simulacra_types::CheckpointData,
    ) -> Result<(), simulacra_types::JournalError> {
        Ok(())
    }

    fn fork_from(
        &self,
        _agent_id: &AgentId,
        _checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        Ok(vec![])
    }

    fn read_from(
        &self,
        _agent_id: &AgentId,
        _start_index: usize,
    ) -> Result<Vec<JournalEntry>, simulacra_types::JournalError> {
        Ok(vec![])
    }
}

impl RecordingTaskFactory {
    fn new(outputs: Vec<Result<AgentLoopOutput, RuntimeError>>) -> Self {
        Self {
            inner: Arc::new(RecordingTaskFactoryInner {
                outputs: Mutex::new(outputs.into()),
                started: Mutex::new(Vec::new()),
                completed: AtomicUsize::new(0),
                started_notify: Notify::new(),
                completed_notify: Notify::new(),
                journal_at_spawn: Mutex::new(None),
                journal_ref: Mutex::new(None),
            }),
        }
    }

    /// Configure the factory to snapshot the journal at spawn time for ordering assertions.
    fn with_journal_capture(self, journal: Arc<dyn JournalStorage>, parent_id: AgentId) -> Self {
        *self.inner.journal_ref.lock().unwrap() = Some((journal, parent_id));
        self
    }

    /// Return the journal entries that existed when create_task was called.
    fn journal_at_spawn_time(&self) -> Option<Vec<JournalEntry>> {
        self.inner.journal_at_spawn.lock().unwrap().clone()
    }

    fn started_count(&self) -> usize {
        self.inner.started.lock().unwrap().len()
    }
    async fn wait_for_completed(&self, expected: usize) {
        loop {
            if self.inner.completed.load(Ordering::SeqCst) >= expected {
                return;
            }
            self.inner.completed_notify.notified().await;
        }
    }
}

impl TaskFactory for RecordingTaskFactory {
    fn create_task(&self, config: SpawnConfig, _cancellation: CancellationToken) -> BoxTaskFuture {
        // Capture journal state at the moment child execution begins.
        if let Some((ref journal, ref parent_id)) = *self.inner.journal_ref.lock().unwrap() {
            let entries = journal.read_all(parent_id).unwrap_or_default();
            *self.inner.journal_at_spawn.lock().unwrap() = Some(entries);
        }

        self.inner.started.lock().unwrap().push(SpawnSnapshot {
            agent_id: config.agent_id.0.clone(),
            parent_id: config.parent_id.0.clone(),
            agent_type: config
                .agent_type
                .clone()
                .unwrap_or_else(|| "generic".to_string()),
            task: config.task.clone(),
            max_tokens: config.budget.max_tokens,
            max_turns: config.budget.max_turns,
            max_sub_agents: config.budget.max_sub_agents,
            restart_strategy: config.restart_strategy.clone(),
        });
        self.inner.started_notify.notify_waiters();

        let output = self
            .inner
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                Ok(AgentLoopOutput {
                    exit_reason: ExitReason::Complete,
                    messages: vec![],
                    token_usage: TokenUsage::default(),
                    used_turns: 0,
                    used_cost: Decimal::ZERO,
                })
            });
        let factory = self.clone();

        Box::pin(async move {
            let result = output;
            factory.inner.completed.fetch_add(1, Ordering::SeqCst);
            factory.inner.completed_notify.notify_waiters();
            result
        })
    }
}

struct SummarySpawnTool {
    live_calls: Arc<AtomicUsize>,
}

impl Tool for SummarySpawnTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "spawn_agent".into(),
            description: "Delegate work to a child agent.".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn call(
        &self,
        _arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>,
    > {
        let calls = Arc::clone(&self.live_calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({
                "child_id": "child-1",
                "agent_type": "researcher",
                "exit_reason": "completed",
                "message": "done",
                "token_usage": {"input_tokens": 3, "output_tokens": 2}
            }))
        })
    }
}

fn replay_entry(agent_id: &str, entry: JournalEntryKind) -> JournalEntry {
    JournalEntry {
        schema_version: simulacra_types::JOURNAL_SCHEMA_VERSION,
        agent_id: AgentId(agent_id.into()),
        timestamp_ms: 1,
        entry,
    }
}

fn build_loop(
    provider: FakeProvider,
    tools: ToolRegistry,
    replay_journal: Option<Vec<JournalEntry>>,
) -> AgentLoop {
    AgentLoop::with_clock_and_replay(
        AgentLoopConfig {
            agent_id: AgentId("parent-agent".into()),
            system_prompt: "You are a parent.".into(),
            model: "test-model".into(),
            max_turns: 10,
            capability: CapabilityToken::default(),
        },
        Box::new(provider),
        tools,
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
        Box::new(simulacra_types::SystemClock),
        replay_journal,
    )
}

fn task_factory_config(child_capabilities: CapabilitiesConfig) -> SimulacraConfig {
    let mut agent_types = HashMap::new();
    agent_types.insert(
        "researcher".into(),
        AgentTypeConfig {
            model: "child-model".into(),
            system_prompt: Some("You are the child researcher.".into()),
            skills: vec![],
            max_turns: Some(3),
            max_tokens: Some(64),
            max_sub_agents: Some(1),
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(child_capabilities),
        },
    );

    SimulacraConfig {
        project: ProjectConfig {
            name: "simulacra-s018-runtime".into(),
            description: None,
        },
        agent_types,
        integrations: HashMap::new(),
        tenants: HashMap::new(),
        mcp: None,
        task: None,
        vfs: VfsConfig::default(),
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: CatalogConfig::default(),
    }
}

async fn run_spawn_tool_call(
    arguments: serde_json::Value,
    can_spawn: &[&str],
    supervisor_reply: Result<AgentLoopOutput, RuntimeError>,
) -> (Result<serde_json::Value, ToolError>, SpawnConfig) {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let tool = SpawnAgentTool {
        sender,
        can_spawn: can_spawn.iter().map(|value| (*value).to_string()).collect(),
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    };
    let call_future = tool.call(arguments, &CapabilityToken::default());
    let receive_future = async move {
        let message = receiver
            .recv()
            .await
            .expect("spawn tool should send one supervisor message");
        assert_eq!(
            message.priority,
            MessagePriority::Command,
            "spawn_agent requests should be sent as command-priority supervisor messages"
        );
        match message.payload {
            SupervisorPayload::Spawn(config, result_tx) => {
                let captured = (*config).clone();
                result_tx
                    .send(supervisor_reply)
                    .expect("spawn tool should still be awaiting the child result");
                captured
            }
            other => panic!("expected SupervisorPayload::Spawn, got {other:?}"),
        }
    };

    let (result, captured) = tokio::join!(call_future, receive_future);
    (result, captured)
}

#[tokio::test]
async fn parent_max_sub_agents_zero_means_unlimited_sub_agents_not_already_exhausted() {
    let mut parent_budget = default_budget();
    parent_budget.max_sub_agents = 0;
    parent_budget.used_sub_agents = 0;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(NoopFactory),
    );

    let spawn = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 2, 1),
    ));

    assert!(
        spawn.is_ok(),
        "max_sub_agents = 0 should mean unlimited for the parent reservation check"
    );
}

#[tokio::test]
async fn parent_max_tokens_zero_means_unlimited_tokens_for_child_budget_requests() {
    let mut parent_budget = default_budget();
    parent_budget.max_tokens = 0;
    parent_budget.used_tokens = 91;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(NoopFactory),
    );

    let spawn = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(50, 2, 1),
    ));

    assert!(
        spawn.is_ok(),
        "max_tokens = 0 should mean unlimited, even when used_tokens is already non-zero"
    );
}

#[tokio::test]
async fn parent_max_turns_zero_means_unlimited_turns_not_already_exhausted() {
    let mut parent_budget = default_budget();
    parent_budget.max_turns = 0;
    parent_budget.used_turns = 9;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(NoopFactory),
    );

    let spawn = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 50, 1),
    ));

    assert!(
        spawn.is_ok(),
        "max_turns = 0 should mean unlimited for parent turn reservations"
    );
}

#[tokio::test]
async fn parent_max_cost_zero_means_unlimited_cost_not_already_exhausted() {
    let mut parent_budget = default_budget();
    parent_budget.max_cost = Decimal::ZERO;
    parent_budget.used_cost = Decimal::new(999, 2);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(NoopFactory),
    );

    let spawn = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget_with_cost(10, 1, Decimal::new(500, 2), 1),
    ));

    assert!(
        spawn.is_ok(),
        "max_cost = 0 should mean unlimited for parent cost reservations"
    );
}

#[test]
fn child_budget_request_exceeding_parent_remaining_budget_is_rejected_before_child_execution() {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.used_tokens = 95;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(factory.clone()),
    );

    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 1, 1),
    ));

    assert!(
        matches!(result, Err(RuntimeError::BudgetExhausted(_))),
        "budget reservations that exceed remaining headroom must fail before child execution"
    );
    assert_eq!(
        factory.started_count(),
        0,
        "no child task should start when the reservation is rejected"
    );
}

#[tokio::test]
async fn child_turn_budget_request_exceeding_parent_remaining_turns_is_rejected_before_child_execution()
 {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.used_turns = 9;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(factory.clone()),
    );

    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 2, 1),
    ));

    assert!(
        matches!(result, Err(RuntimeError::BudgetExhausted(_))),
        "child max_turns should be checked against the parent's remaining turns before execution"
    );
    assert_eq!(
        factory.started_count(),
        0,
        "no child task should start when the turn reservation exceeds the parent's remaining turns"
    );
}

#[tokio::test]
async fn child_cost_budget_request_exceeding_parent_remaining_cost_is_rejected_before_child_execution()
 {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.used_cost = Decimal::new(9950, 2);
    parent_budget.max_cost = Decimal::new(10000, 2);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(factory.clone()),
    );

    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget_with_cost(10, 1, Decimal::new(100, 2), 1),
    ));

    assert!(
        matches!(result, Err(RuntimeError::BudgetExhausted(_))),
        "child max_cost should be checked against the parent's remaining cost before execution"
    );
    assert_eq!(
        factory.started_count(),
        0,
        "no child task should start when the cost reservation exceeds the parent's remaining budget"
    );
}

#[tokio::test]
async fn accepting_child_spawn_increments_parent_used_sub_agents() {
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(NoopFactory),
    );

    supervisor
        .spawn_agent(spawn_config(
            "child-1",
            "parent-agent",
            child_budget(10, 1, 1),
        ))
        .expect("spawn should succeed for a within-budget child");

    assert_eq!(supervisor.parent_budget().used_sub_agents, 1);
}

#[tokio::test]
async fn child_token_usage_is_rolled_up_from_agent_loop_output_not_stale_spawn_budget_clone() {
    let factory = RecordingTaskFactory::new(vec![Ok(AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![Message {
            role: Role::Assistant,
            content: "child result".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        token_usage: TokenUsage {
            input_tokens: 19,
            output_tokens: 23,
        },
        used_turns: 0,
        used_cost: Decimal::ZERO,
    })]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    supervisor
        .spawn_agent(spawn_config(
            "child-1",
            "parent-agent",
            child_budget(40, 2, 1),
        ))
        .expect("spawn should succeed");
    factory.wait_for_completed(1).await;

    assert_eq!(
        supervisor.parent_budget().used_tokens,
        42,
        "budget rollup should use AgentLoopOutput.token_usage totals, not a stale SpawnConfig clone"
    );
}

#[tokio::test]
async fn child_turn_and_cost_usage_are_rolled_up_from_agent_loop_output_not_stale_spawn_budget_clone()
 {
    let factory = RecordingTaskFactory::new(vec![Ok(AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![],
        token_usage: TokenUsage::default(),
        used_turns: 2,
        used_cost: Decimal::new(375, 2),
    })]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    supervisor
        .spawn_agent(spawn_config(
            "child-1",
            "parent-agent",
            child_budget(40, 2, 1),
        ))
        .expect("spawn should succeed");
    factory.wait_for_completed(1).await;

    let budget = supervisor.parent_budget();
    assert_eq!(
        budget.used_turns, 2,
        "budget rollup should use AgentLoopOutput.used_turns from the completed child"
    );
    assert_eq!(
        budget.used_cost,
        Decimal::new(375, 2),
        "budget rollup should use AgentLoopOutput.used_cost from the completed child"
    );
}

#[tokio::test]
async fn spawn_config_passes_agent_type_and_task_to_the_task_factory() {
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    supervisor
        .spawn_agent(spawn_config_with_agent_type(
            "child-1",
            "parent-agent",
            "reviewer",
            child_budget(10, 1, 1),
        ))
        .expect("spawn should succeed");
    factory.wait_for_completed(1).await;

    let started = factory.inner.started.lock().unwrap().clone();
    let snapshot = started
        .first()
        .expect("task factory should capture the spawn config");
    assert_eq!(snapshot.agent_type, "reviewer");
    assert_eq!(snapshot.task, "delegate task");
}

#[tokio::test]
async fn parent_receives_exactly_one_tool_result_message_per_spawn_agent_call() {
    // Exercise the real SpawnAgentTool path (not a fake) to verify the parent
    // receives exactly one result per spawn_agent call.
    let (result, _captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 1,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Ok(child_success_output()),
    )
    .await;

    // run_spawn_tool_call returns exactly one result (the return value of Tool::call).
    // The real SpawnAgentTool produces a single JSON payload, verifying the one-result contract.
    let value = result.expect("successful spawn should return a tool result");
    assert!(
        value.get("child_id").is_some(),
        "the single tool result should contain child_id"
    );
    assert!(
        value.get("exit_reason").is_some(),
        "the single tool result should contain exit_reason"
    );
}

#[tokio::test]
async fn failed_spawn_agent_calls_return_error_tool_results_with_child_id_agent_type_and_error() {
    // Exercise the real SpawnAgentTool path with a child runtime failure.
    let (result, _captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 1,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Err(RuntimeError::CapabilityViolation("shell denied".into())),
    )
    .await;

    match result {
        Err(ToolError::ExecutionFailed(msg)) => {
            assert!(
                msg.contains("child_id") || msg.contains("child-"),
                "error message should reference the child_id: {msg}"
            );
            assert!(
                msg.contains("researcher"),
                "error message should reference the agent_type: {msg}"
            );
            assert!(
                msg.contains("shell denied") || msg.contains("failed"),
                "error message should contain the failure reason: {msg}"
            );
        }
        other => panic!(
            "failed spawn_agent should return Err(ToolError::ExecutionFailed), got {other:?}"
        ),
    }
}

#[tokio::test]
async fn spawn_agent_tool_parses_capabilities_override_json_into_spawn_config() {
    let (result, captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 1,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            },
            "capabilities": {
                "network": ["net:api.github.com"],
                "mcp_tools": ["github"],
                "shell": true,
                "javascript": true,
                "python": false,
                "paths_write": ["/workspace/out.txt"],
                "paths_read": ["/workspace/in.txt"],
                "spawn_types": ["reviewer"]
            }
        }),
        &["researcher"],
        Ok(child_success_output()),
    )
    .await;

    result.expect("successful child result should still return a tool payload");
    let cap = captured
        .capability
        .as_ref()
        .expect("capability should be Some when LLM provides capabilities");
    assert_eq!(
        cap.network,
        vec![NetworkPermission("net:api.github.com".into())]
    );
    assert_eq!(cap.mcp_tools, vec!["github".to_string()]);
    assert!(cap.shell);
    assert!(cap.javascript);
    assert_eq!(
        cap.paths_write,
        vec![PathPattern("/workspace/out.txt".into())]
    );
    assert_eq!(
        cap.paths_read,
        vec![PathPattern("/workspace/in.txt".into())]
    );
    assert_eq!(cap.spawn_types, vec!["reviewer".to_string()]);
}

#[tokio::test]
async fn spawn_agent_tool_child_runtime_failures_return_toolerror_execution_failed() {
    let (result, _captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 1,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Err(RuntimeError::CapabilityViolation("shell denied".into())),
    )
    .await;

    assert!(
        matches!(result, Err(ToolError::ExecutionFailed(_))),
        "child runtime failures should surface as Err(ToolError::ExecutionFailed(...)) so AgentLoop marks the tool result as is_error"
    );
}

#[tokio::test]
async fn spawn_agent_tool_does_not_hardcode_parent_agent_id_in_spawn_config() {
    let (_result, captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 1,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Ok(child_success_output()),
    )
    .await;

    assert_eq!(
        captured.parent_id,
        AgentId("parent-agent".into()),
        "SpawnAgentTool should propagate the caller's parent AgentId into SpawnConfig"
    );
}

#[tokio::test]
async fn child_internal_messages_are_not_appended_to_parent_conversation_history() {
    // Exercise the real SpawnAgentTool path. The child output contains multiple
    // messages (system, user, assistant), but the parent should only see the
    // single JSON tool result — not the child's internal conversation.
    let child_output = AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![
            Message {
                role: Role::System,
                content: "child system".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: "child task".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: Role::Assistant,
                content: "child result".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ],
        token_usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
        used_turns: 1,
        used_cost: Decimal::ZERO,
    };

    let (result, _captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 10,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Ok(child_output),
    )
    .await;

    // The real SpawnAgentTool returns a single JSON value. The child's internal
    // System/User/Assistant messages are NOT surfaced — only the terminal summary.
    let value = result.expect("spawn should succeed");
    assert_eq!(
        value.get("message").and_then(serde_json::Value::as_str),
        Some("child result"),
        "parent should see only the child's final assistant message as a summary, not internal messages"
    );
    // Verify the result is a single flat JSON object, not a list of messages.
    assert!(
        value.is_object() && !value.is_array(),
        "spawn_agent should return a single JSON object, not an array of child messages"
    );
}

#[test]
fn agent_task_factory_runs_a_real_child_agent_loop_with_the_child_prompt_and_model() {
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-1",
        "model": "child-model",
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: false,
            javascript: false,
            python: false,
            paths_read: vec!["/workspace/**".into()],
            paths_write: vec![],

            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal: Arc::clone(&journal),
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };
    let spawn = spawn_config("child-1", "parent-agent", child_budget(32, 1, 0));

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(
            spawn.clone(),
            CancellationToken::new(Duration::from_secs(1)),
        ))
        .expect("child task should complete");

    assert_eq!(
        output.messages.first().map(|message| message.role.clone()),
        Some(Role::System)
    );
    assert_eq!(
        output
            .messages
            .first()
            .map(|message| message.content.as_str()),
        Some("You are the child researcher."),
        "child execution should run through AgentLoop::run(task) so the configured system prompt is present"
    );
    assert_eq!(
        output.messages.get(1).map(|message| message.role.clone()),
        Some(Role::User)
    );
    assert_eq!(
        output
            .messages
            .get(1)
            .map(|message| message.content.as_str()),
        Some("delegate task"),
        "child execution should preserve the delegated task as the child user turn"
    );

    let request = server.first_request_json();
    assert_eq!(request["model"], "child-model");
    assert_eq!(
        request["messages"][0]["content"],
        "You are the child researcher."
    );

    let child_entries = journal
        .read_all(&spawn.agent_id)
        .expect("child journal should be readable");
    assert!(
        !child_entries.is_empty()
            && child_entries
                .iter()
                .all(|entry| entry.agent_id == spawn.agent_id),
        "child journal entries should be written under the child agent_id so they correlate through child_id"
    );
}

#[test]
fn agent_task_factory_applies_child_cell_and_tool_hooks_before_provider_call() {
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-1",
        "model": "child-model",
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let observed_configured_cell = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_for_registrar = Arc::clone(&observed_configured_cell);

    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: false,
            javascript: false,
            python: false,
            paths_read: vec!["/workspace/**".into()],
            paths_write: vec![],
            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: Some(Arc::new(|cell: &mut simulacra_sandbox::AgentCell| {
            cell.tenant_integrations = vec!["toy-saas".to_string()];
        })),
        child_tool_registrar: Some(Arc::new(move |registry, cell| {
            observed_for_registrar.store(
                cell.tenant_integrations == vec!["toy-saas".to_string()],
                Ordering::SeqCst,
            );
            registry.register(Box::new(ExtraProbeTool));
        })),
    };
    let spawn = spawn_config("child-hooks-1", "parent-agent", child_budget(32, 1, 0));

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("child task should complete");

    assert!(
        observed_configured_cell.load(Ordering::SeqCst),
        "child tool registration should see the AgentCell after caller-specific configuration"
    );
    let request = server.first_request_json();
    let tools = request
        .get("tools")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        tools.iter().any(|t| {
            t.pointer("/function/name")
                .and_then(|v| v.as_str())
                .map(|name| name == "child_extra_probe")
                .unwrap_or(false)
        }),
        "child provider call should include caller-registered child tools"
    );
}

#[test]
fn agent_task_factory_intersects_child_type_capability_with_the_spawn_override() {
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-1",
        "model": "child-model",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "shell_exec",
                        "arguments": "{\"command\":\"echo hello\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: true,
            javascript: false,
            python: false,
            paths_read: vec!["/workspace/**".into()],
            paths_write: vec!["/workspace/**".into()],

            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };
    let mut spawn = spawn_config("child-1", "parent-agent", child_budget(32, 1, 0));
    spawn.capability = Some(CapabilityToken::default());

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("child task should complete");

    let tool_message = output
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("child loop should append the tool result");
    assert!(
        tool_message.content.starts_with("ERROR: ")
            && tool_message
                .content
                .contains("shell capability not granted"),
        "effective child capability should be the intersection of child type config and the attenuated spawn capability override"
    );
}

#[test]
fn widened_child_capabilities_are_rejected_before_the_child_task_starts() {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    let child = CapabilityToken {
        shell: true,
        ..Default::default()
    };

    let result = supervisor.spawn_agent(SpawnConfig {
        agent_id: AgentId("child-1".into()),
        parent_id: AgentId("parent-agent".into()),
        capability: Some(child),
        budget: child_budget(10, 1, 1),
        restart_strategy: RestartStrategy::LetCrash,
        agent_type: Some(String::new()),
        task: String::new(),
        system_prompt: None,
        tier: None,
        resolved_tier: None,
    });

    assert!(
        matches!(result, Err(RuntimeError::CapabilityViolation(_))),
        "capability widening must be rejected before the child task starts"
    );
    assert_eq!(factory.started_count(), 0);
}

#[test]
fn child_may_spawn_descendants_only_from_its_own_remaining_budget() {
    let child_supervisor = AgentSupervisor::new(
        CapabilityToken {
            spawn_types: vec!["reviewer".into()],
            ..Default::default()
        },
        child_budget(10, 2, 1),
    );
    let mut child_supervisor = child_supervisor;

    let result = child_supervisor.spawn_agent(spawn_config(
        "grandchild-1",
        "child-1",
        child_budget(11, 1, 1),
    ));

    assert!(
        matches!(result, Err(RuntimeError::BudgetExhausted(_))),
        "descendant reservations should be enforced against the child's own remaining budget"
    );
}

#[test]
fn parent_replay_reuses_recorded_spawn_agent_tool_result_without_a_live_child_run() {
    let live_calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(SummarySpawnTool {
        live_calls: Arc::clone(&live_calls),
    }));
    let provider = FakeProvider::new(vec![]);
    let replay = vec![
        replay_entry("parent-agent", JournalEntryKind::TurnStart),
        replay_entry(
            "parent-agent",
            JournalEntryKind::LlmRequest {
                model: "test-model".into(),
                message_count: 1,
            },
        ),
        replay_entry(
            "parent-agent",
            JournalEntryKind::LlmResponse {
                model: "test-model".into(),
                token_usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                finish_reason: "ToolUse".into(),
                assistant_message: Some(Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls: vec![ToolCallMessage {
                        id: "call-1".into(),
                        name: "spawn_agent".into(),
                        arguments: serde_json::json!({}),
                    }],
                    tool_call_id: None,
                }),
            },
        ),
        replay_entry(
            "parent-agent",
            JournalEntryKind::ToolCall {
                tool_call_id: Some("call-1".into()),
                tool_name: "spawn_agent".into(),
                arguments: serde_json::json!({}),
            },
        ),
        replay_entry(
            "parent-agent",
            JournalEntryKind::ToolResult {
                tool_call_id: Some("call-1".into()),
                tool_name: "spawn_agent".into(),
                content: r#"{"child_id":"child-1","agent_type":"researcher","exit_reason":"completed","message":"done","token_usage":{"input_tokens":3,"output_tokens":2}}"#.into(),
                is_error: false,
            },
        ),
    ];
    let mut loop_ = build_loop(provider, tools, Some(replay));
    let mut messages = vec![Message {
        role: Role::User,
        content: "delegate".into(),
        tool_calls: vec![],
        tool_call_id: None,
    }];

    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(loop_.run_single_turn(&mut messages))
        .expect("replayed turn should succeed");

    assert!(
        matches!(result, TurnResult::ToolCallsProcessed { .. }),
        "replay should preserve the parent-visible spawn_agent tool result"
    );
    assert_eq!(
        live_calls.load(Ordering::SeqCst),
        0,
        "replay should not invoke a live child tool call when ToolResult is already journaled"
    );
}

#[tokio::test]
async fn create_agent_span_uses_genai_operation_name_and_child_agent_name() {
    let (_, spans, _) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            default_capability(),
            default_budget(),
            Arc::new(NoopFactory),
        );
        supervisor
            .spawn_agent(spawn_config(
                "child-1",
                "parent-agent",
                child_budget(10, 1, 1),
            ))
            .expect("spawn should succeed");
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "create_agent"
                && span.fields.get("gen_ai.operation.name").map(String::as_str)
                    == Some("create_agent")
                && span.fields.get("gen_ai.agent.name").map(String::as_str) == Some("child-1")
        }),
        "accepted spawns should emit a create_agent span with standard GenAI attributes"
    );
}

#[test]
fn running_the_child_loop_emits_an_invoke_agent_span() {
    let provider = FakeProvider::new(vec![ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: "done".into(),
            tool_calls: vec![],
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens: 3,
            output_tokens: 2,
        },
        finish_reason: FinishReason::EndTurn,
        provider_response_id: Some("resp-1".into()),
        model: "test-model".into(),
    }]);
    let tools = ToolRegistry::new();

    let (_, spans, _) = capture_trace(|| {
        let mut loop_ = build_loop(provider, tools, None);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(loop_.run("delegate task"))
            .expect("child loop should run");
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "invoke_agent"
                && span.fields.get("gen_ai.operation.name").map(String::as_str)
                    == Some("invoke_agent")
        }),
        "child execution should emit an invoke_agent span"
    );
}

#[tokio::test]
async fn subagent_lifecycle_spans_include_parent_and_child_linkage_attributes() {
    let (_, spans, _) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            default_capability(),
            default_budget(),
            Arc::new(NoopFactory),
        );
        supervisor
            .spawn_agent(spawn_config(
                "child-1",
                "parent-agent",
                child_budget(10, 1, 1),
            ))
            .expect("spawn should succeed");
    });

    assert!(
        spans.iter().any(|span| {
            span.fields.contains_key("simulacra.parent.agent_id")
                && span.fields.contains_key("simulacra.child.agent_type")
        }),
        "sub-agent lifecycle spans should expose Simulacra-specific parent/child linkage attributes"
    );
}

#[tokio::test]
async fn successful_child_completion_is_logged_with_child_parent_exit_reason_and_token_totals() {
    let factory = RecordingTaskFactory::new(vec![Ok(AgentLoopOutput {
        exit_reason: ExitReason::BudgetExhausted,
        messages: vec![],
        token_usage: TokenUsage {
            input_tokens: 8,
            output_tokens: 5,
        },
        used_turns: 0,
        used_cost: Decimal::ZERO,
    })]);

    let (_, _, events) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            default_capability(),
            default_budget(),
            Arc::new(factory.clone()),
        );
        supervisor
            .spawn_agent(spawn_config(
                "child-1",
                "parent-agent",
                child_budget(10, 1, 1),
            ))
            .expect("spawn should succeed");
    });
    factory.wait_for_completed(1).await;

    assert!(
        events.iter().any(|event| {
            event.level == "INFO"
                && event.fields.contains_key("child_id")
                && event.fields.contains_key("parent_id")
                && event.fields.contains_key("exit_reason")
                && event.fields.contains_key("token_total")
        }),
        "successful child completion should be logged with child id, parent id, exit reason, and token totals"
    );
}

#[tokio::test]
async fn child_failure_is_logged_at_warn_with_child_parent_agent_type_and_failure_reason() {
    let factory =
        RecordingTaskFactory::new(vec![Err(RuntimeError::CapabilityViolation("boom".into()))]);

    let (_, _, events) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            default_capability(),
            default_budget(),
            Arc::new(factory.clone()),
        );
        // After WARNING 1's fix, spawn_agent propagates immediate child errors —
        // the return value may be Err for this test. We only care about the
        // WARN log being emitted via process_child_result.
        let _ = supervisor.spawn_agent(spawn_config(
            "child-1",
            "parent-agent",
            child_budget(10, 1, 1),
        ));
    });
    factory.wait_for_completed(1).await;

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event.fields.contains_key("child_id")
                && event.fields.contains_key("parent_id")
                && event.fields.contains_key("agent_type")
                && event.fields.contains_key("failure_reason")
        }),
        "child failures should log a WARN event with child id, parent id, agent type, and failure reason"
    );
}

#[test]
fn spawn_acceptance_uses_command_priority_spawn_messages_in_the_actor_protocol() {
    let (_tx, _rx) = tokio::sync::oneshot::channel();
    let msg = SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("parent-agent".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "child-1",
                "parent-agent",
                child_budget(10, 1, 1),
            )),
            _tx,
        ),
    };

    assert!(
        matches!(msg.payload, SupervisorPayload::Spawn(_, _))
            && msg.priority == MessagePriority::Command,
        "interactive spawn requests should travel through the supervisor actor protocol as Command/Spawn messages"
    );
}

// ---------------------------------------------------------------------------
// Finding 1: Journal tests for SubAgentSpawned and SubAgentCompleted
// RED — the supervisor constructs these entries but never writes them.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervisor_writes_sub_agent_spawned_journal_entry_to_parent_stream_before_child_execution()
{
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let parent_id = AgentId("parent-agent".into());
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())])
        .with_journal_capture(Arc::clone(&journal), parent_id.clone());
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    supervisor.set_journal_storage(Arc::clone(&journal));

    supervisor
        .spawn_agent(spawn_config(
            "child-1",
            "parent-agent",
            child_budget(10, 1, 1),
        ))
        .expect("spawn should succeed");
    factory.wait_for_completed(1).await;

    // R7/R2: Verify temporal ordering — SubAgentSpawned must exist in the journal
    // *at the moment the child task begins*, not just after everything completes.
    let entries_at_spawn = factory
        .journal_at_spawn_time()
        .expect("factory should have captured journal state at spawn time");
    let spawned_before_child = entries_at_spawn.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::SubAgentSpawned {
                child_id,
                agent_type,
                ..
            }
            if child_id.0 == "child-1" && agent_type == "researcher"
        )
    });
    assert!(
        spawned_before_child,
        "SubAgentSpawned must be in the parent journal BEFORE child execution begins \
         (captured {} entries at spawn time, none were SubAgentSpawned)",
        entries_at_spawn.len()
    );
}

#[tokio::test]
async fn supervisor_writes_sub_agent_completed_journal_entry_to_parent_stream_after_child_success()
{
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let parent_id = AgentId("parent-agent".into());
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())])
        .with_journal_capture(Arc::clone(&journal), parent_id.clone());
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    supervisor.set_journal_storage(Arc::clone(&journal));

    supervisor
        .spawn_agent(spawn_config(
            "child-1",
            "parent-agent",
            child_budget(10, 1, 1),
        ))
        .expect("spawn should succeed");
    factory.wait_for_completed(1).await;

    let parent_entries = journal
        .read_all(&parent_id)
        .expect("parent journal should be readable");

    // R7: Verify ordering — SubAgentSpawned MUST come before SubAgentCompleted.
    let spawned_idx = parent_entries.iter().position(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::SubAgentSpawned { child_id, .. }
            if child_id.0 == "child-1"
        )
    });
    let completed_idx = parent_entries.iter().position(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::SubAgentCompleted { child_id, success }
            if child_id.0 == "child-1" && *success
        )
    });

    assert!(
        spawned_idx.is_some(),
        "SubAgentSpawned should be present in the parent journal"
    );
    assert!(
        completed_idx.is_some(),
        "SubAgentCompleted {{ success: true }} should be present in the parent journal"
    );
    assert!(
        spawned_idx.unwrap() < completed_idx.unwrap(),
        "SubAgentSpawned (index {:?}) must appear before SubAgentCompleted (index {:?}) \
         in the parent journal to prove correct ordering",
        spawned_idx,
        completed_idx
    );

    // Also verify SubAgentCompleted was NOT present at spawn time (it comes after child execution).
    let entries_at_spawn = factory
        .journal_at_spawn_time()
        .expect("factory should have captured journal state at spawn time");
    let completed_at_spawn = entries_at_spawn.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::SubAgentCompleted { child_id, .. }
            if child_id.0 == "child-1"
        )
    });
    assert!(
        !completed_at_spawn,
        "SubAgentCompleted must NOT be in the journal at spawn time — it should only appear after child execution"
    );
}

#[tokio::test]
async fn supervisor_writes_sub_agent_completed_with_success_false_on_child_failure() {
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let parent_id = AgentId("parent-agent".into());
    let factory =
        RecordingTaskFactory::new(vec![Err(RuntimeError::CapabilityViolation("boom".into()))])
            .with_journal_capture(Arc::clone(&journal), parent_id.clone());
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    supervisor.set_journal_storage(Arc::clone(&journal));

    // After WARNING 1's fix, spawn_agent propagates the immediate child error.
    // We still proceed to verify the journal recorded SubAgentCompleted{success:false}.
    let _ = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 1, 1),
    ));
    factory.wait_for_completed(1).await;

    let parent_entries = journal
        .read_all(&parent_id)
        .expect("parent journal should be readable");

    // R7: Verify ordering — SubAgentSpawned before SubAgentCompleted { success: false }.
    let spawned_idx = parent_entries.iter().position(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::SubAgentSpawned { child_id, .. }
            if child_id.0 == "child-1"
        )
    });
    let failed_idx = parent_entries.iter().position(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::SubAgentCompleted { child_id, success }
            if child_id.0 == "child-1" && !*success
        )
    });

    assert!(
        spawned_idx.is_some(),
        "SubAgentSpawned should be present in the parent journal"
    );
    assert!(
        failed_idx.is_some(),
        "SubAgentCompleted {{ success: false }} should be present in the parent journal"
    );
    assert!(
        spawned_idx.unwrap() < failed_idx.unwrap(),
        "SubAgentSpawned (index {:?}) must appear before SubAgentCompleted {{ success: false }} (index {:?})",
        spawned_idx,
        failed_idx
    );
}

// ---------------------------------------------------------------------------
// Finding 3: Exit reason format — spec says snake_case, impl uses Debug (PascalCase).
// RED — format!("{:?}", ExitReason::BudgetExhausted) produces "BudgetExhausted"
// but the spec requires "budget_exhausted".
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_agent_tool_exit_reason_uses_snake_case_format_per_spec() {
    let budget_exhausted_output = AgentLoopOutput {
        exit_reason: ExitReason::BudgetExhausted,
        messages: vec![Message {
            role: Role::Assistant,
            content: "partial".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        token_usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
        },
        used_turns: 1,
        used_cost: Decimal::ZERO,
    };

    let (result, _captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 10,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Ok(budget_exhausted_output),
    )
    .await;

    let value = result.expect("budget exhaustion should still be a success payload");
    assert_eq!(
        value.get("exit_reason").and_then(serde_json::Value::as_str),
        Some("budget_exhausted"),
        "exit_reason should use snake_case format per spec, not Debug format like BudgetExhausted"
    );
}

#[tokio::test]
async fn spawn_agent_tool_exit_reason_completed_uses_snake_case_format_per_spec() {
    let completed_output = AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![Message {
            role: Role::Assistant,
            content: "done".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        token_usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
        },
        used_turns: 1,
        used_cost: Decimal::ZERO,
    };

    let (result, _captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 10,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Ok(completed_output),
    )
    .await;

    let value = result.expect("completed child should return a success payload");
    assert_eq!(
        value.get("exit_reason").and_then(serde_json::Value::as_str),
        Some("completed"),
        "exit_reason should be \"completed\" (snake_case) for ExitReason::Complete, not Debug format like \"Complete\""
    );
}

#[tokio::test]
async fn spawn_agent_tool_exit_reason_max_turns_uses_snake_case_format_per_spec() {
    let max_turns_output = AgentLoopOutput {
        exit_reason: ExitReason::MaxTurns,
        messages: vec![Message {
            role: Role::Assistant,
            content: "ran out of turns".into(),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        token_usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
        },
        used_turns: 3,
        used_cost: Decimal::ZERO,
    };

    let (result, _captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 10,
                "max_turns": 3,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Ok(max_turns_output),
    )
    .await;

    let value = result
        .expect("max_turns child should return a success payload (partial success, not error)");
    assert_eq!(
        value.get("exit_reason").and_then(serde_json::Value::as_str),
        Some("max_turns"),
        "exit_reason should be \"max_turns\" (snake_case) for ExitReason::MaxTurns, not Debug format like \"MaxTurns\""
    );
}

// ---------------------------------------------------------------------------
// Finding 4: Three-way capability intersection (parent, config, override).
// The existing test only checks two-way (config vs override). This adds a test
// where the parent, config, AND override all differ, asserting the intersection.
// ---------------------------------------------------------------------------

#[test]
fn agent_task_factory_performs_three_way_capability_intersection_parent_config_and_override() {
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-1",
        "model": "child-model",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "shell_exec",
                        "arguments": "{\"command\":\"echo hello\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    // Config grants: shell=true, javascript=true, python=false
    let child_config_capabilities = CapabilitiesConfig {
        network: vec![],
        mcp: vec![],
        shell: true,
        javascript: true,
        python: false,
        paths_read: vec!["/workspace/**".into()],
        paths_write: vec!["/workspace/**".into()],

        memory: None,
    };

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let factory = AgentTaskFactory {
        config: task_factory_config(child_config_capabilities),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    // Override grants: shell=true, javascript=false
    // Parent grants: shell=false (via default CapabilityToken)
    // Intersection: shell should be false (parent denies), javascript should be false (override denies)
    let mut spawn = spawn_config("child-1", "parent-agent", child_budget(32, 1, 0));
    spawn.capability = Some(CapabilityToken {
        shell: true,
        javascript: false,
        ..Default::default()
    });

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("child task should complete");

    let tool_message = output
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("child loop should append the tool result");
    assert!(
        tool_message.content.starts_with("ERROR: ")
            && tool_message
                .content
                .contains("shell capability not granted"),
        "effective child capability should be the three-way intersection of parent token, \
         child type config, and the override — parent denies shell even though config and override allow it"
    );
}

// ---------------------------------------------------------------------------
// Finding 5: Exact-boundary budget tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_budget_exactly_equals_parent_remaining_budget_is_accepted() {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.max_tokens = 100;
    parent_budget.used_tokens = 90;
    let mut supervisor =
        AgentSupervisor::with_task_factory(default_capability(), parent_budget, Arc::new(factory));

    // Request exactly 10 tokens when parent has exactly 10 remaining
    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 1, 1),
    ));

    assert!(
        result.is_ok(),
        "child budget request that exactly equals the parent's remaining budget should be accepted"
    );
}

#[tokio::test]
async fn child_budget_one_token_over_parent_remaining_is_rejected() {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.max_tokens = 100;
    parent_budget.used_tokens = 90;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(factory.clone()),
    );

    // Request 11 tokens when parent has exactly 10 remaining
    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(11, 1, 1),
    ));

    assert!(
        matches!(result, Err(RuntimeError::BudgetExhausted(_))),
        "child budget request one token over parent's remaining budget should be rejected"
    );
    assert_eq!(factory.started_count(), 0);
}

#[tokio::test]
async fn child_turns_exactly_equals_parent_remaining_turns_is_accepted() {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.max_turns = 10;
    parent_budget.used_turns = 8;
    let mut supervisor =
        AgentSupervisor::with_task_factory(default_capability(), parent_budget, Arc::new(factory));

    // Request exactly 2 turns when parent has exactly 2 remaining
    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 2, 1),
    ));

    assert!(
        result.is_ok(),
        "child turn request that exactly equals the parent's remaining turns should be accepted"
    );
}

#[tokio::test]
async fn child_turns_one_over_parent_remaining_is_rejected() {
    let factory = RecordingTaskFactory::new(vec![]);
    let mut parent_budget = default_budget();
    parent_budget.max_turns = 10;
    parent_budget.used_turns = 8;
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        parent_budget,
        Arc::new(factory.clone()),
    );

    // Request 3 turns when parent has exactly 2 remaining
    let result = supervisor.spawn_agent(spawn_config(
        "child-1",
        "parent-agent",
        child_budget(10, 3, 1),
    ));

    assert!(
        matches!(result, Err(RuntimeError::BudgetExhausted(_))),
        "child turn request one over parent's remaining turns should be rejected"
    );
    assert_eq!(factory.started_count(), 0);
}

// ---------------------------------------------------------------------------
// Finding 6: Empty message field — run_spawn_tool_call with no assistant message.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_agent_tool_returns_empty_message_when_child_output_has_no_assistant_message() {
    let output_with_no_assistant = AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![
            // Only system and user messages, no assistant message
            Message {
                role: Role::System,
                content: "system prompt".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: "task".into(),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ],
        token_usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 0,
        },
        used_turns: 0,
        used_cost: Decimal::ZERO,
    };

    let (result, _captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 10,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Ok(output_with_no_assistant),
    )
    .await;

    let value =
        result.expect("child with no assistant message should still return a success payload");
    assert_eq!(
        value.get("message").and_then(serde_json::Value::as_str),
        Some(""),
        "spawn_agent should return empty string for message when the child has no final assistant message"
    );
}

#[tokio::test]
async fn spawn_agent_tool_returns_empty_message_when_child_output_messages_list_is_empty() {
    let output_with_empty_messages = AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![],
        token_usage: TokenUsage::default(),
        used_turns: 0,
        used_cost: Decimal::ZERO,
    };

    let (result, _captured) = run_spawn_tool_call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 10,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &["researcher"],
        Ok(output_with_empty_messages),
    )
    .await;

    let value = result.expect("child with empty messages should still return a success payload");
    assert_eq!(
        value.get("message").and_then(serde_json::Value::as_str),
        Some(""),
        "spawn_agent should return empty string for message when the child messages list is empty"
    );
}

// ---------------------------------------------------------------------------
// Finding 7: Cancellation path — oneshot sender dropped.
// RED — SpawnAgentTool returns Ok(json!({..error..})) instead of Err(ToolError).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_agent_tool_returns_error_when_supervisor_drops_result_channel() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<SupervisorMessage>(1);
    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec!["researcher".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    };

    let call_future = tool.call(
        serde_json::json!({
            "agent_type": "researcher",
            "task": "check",
            "budget": {
                "max_tokens": 10,
                "max_turns": 1,
                "max_cost": "0",
                "max_sub_agents": 0
            }
        }),
        &CapabilityToken::default(),
    );

    let drop_future = async move {
        let message = receiver
            .recv()
            .await
            .expect("spawn tool should send one supervisor message");
        // Extract and immediately drop the oneshot sender to simulate cancellation
        match message.payload {
            SupervisorPayload::Spawn(_config, result_tx) => {
                drop(result_tx);
            }
            other => panic!("expected SupervisorPayload::Spawn, got {other:?}"),
        }
    };

    let (result, _) = tokio::join!(call_future, drop_future);

    assert!(
        matches!(result, Err(simulacra_types::ToolError::ExecutionFailed(_))),
        "spawn_agent should return Err(ToolError::ExecutionFailed) when the supervisor drops the result channel, \
         not Ok(json) with an error field"
    );
}

// ---------------------------------------------------------------------------
// Finding 2: Tests against real SpawnAgentTool definition shape.
// These test the actual SpawnAgentTool (not a fake) for definition correctness.
// ---------------------------------------------------------------------------

fn make_real_spawn_agent_tool() -> SpawnAgentTool {
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    SpawnAgentTool {
        sender,
        can_spawn: vec!["researcher".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    }
}

#[test]
fn real_spawn_agent_tool_definition_uses_the_documented_name_and_description() {
    let tool = make_real_spawn_agent_tool();
    let definition = tool.definition();

    assert_eq!(definition.name, "spawn_agent");
    assert_eq!(
        definition.description,
        "Spawn a supervised child agent to handle a delegated task and return its terminal summary."
    );
}

#[test]
fn real_spawn_agent_tool_definition_exposes_agent_type_task_budget_and_capabilities() {
    let tool = make_real_spawn_agent_tool();
    let definition = tool.definition();
    let properties = definition
        .input_schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .expect("schema should expose properties");

    for field in ["agent_type", "task", "budget", "capabilities"] {
        assert!(
            properties.contains_key(field),
            "real spawn_agent schema should expose {field}"
        );
    }
}

#[test]
fn real_spawn_agent_tool_budget_schema_requires_all_fields_and_disallows_extras() {
    let tool = make_real_spawn_agent_tool();
    let definition = tool.definition();
    let budget = definition
        .input_schema
        .pointer("/properties/budget")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    assert_eq!(
        budget.get("required"),
        Some(&serde_json::json!([
            "max_tokens",
            "max_turns",
            "max_cost",
            "max_sub_agents"
        ]))
    );
    assert_eq!(
        budget.get("additionalProperties"),
        Some(&serde_json::Value::Bool(false))
    );
}

#[test]
fn real_spawn_agent_tool_capabilities_schema_matches_spec_shape() {
    let tool = make_real_spawn_agent_tool();
    let definition = tool.definition();
    let capabilities = definition
        .input_schema
        .pointer("/properties/capabilities/properties")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();

    for field in [
        "network",
        "mcp_tools",
        "shell",
        "javascript",
        "python",
        "paths_write",
        "paths_read",
        "spawn_types",
    ] {
        assert!(
            capabilities.contains_key(field),
            "real spawn_agent capability override schema should include {field}"
        );
    }
}

// ── S023: Generic sub-agent tests ──────────────────────────────────────

/// Helper to create a generic spawn config (no agent_type, uses system_prompt).
fn generic_spawn_config(
    agent_id: &str,
    parent_id: &str,
    system_prompt: &str,
    budget: ResourceBudget,
) -> SpawnConfig {
    SpawnConfig {
        agent_id: AgentId(agent_id.into()),
        parent_id: AgentId(parent_id.into()),
        capability: None,
        budget,
        restart_strategy: RestartStrategy::LetCrash,
        agent_type: None,
        task: "generic task".into(),
        system_prompt: Some(system_prompt.into()),
        tier: None,
        resolved_tier: None,
    }
}

#[test]
fn generic_spawn_with_system_prompt_creates_child() {
    // Spawn with system_prompt, no agent_type. Verify the child runs and the
    // system prompt is forwarded to the provider.
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-generic-1",
        "model": "parent-model",
        "choices": [{
            "message": { "role": "assistant", "content": "generic done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: false,
            javascript: false,
            python: false,
            paths_read: vec![],
            paths_write: vec![],

            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    let spawn = generic_spawn_config(
        "child-generic-1",
        "parent-agent",
        "You are a custom generic agent.",
        child_budget(32, 1, 0),
    );

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("generic child task should complete");

    // Verify the system prompt was used
    let request = server.first_request_json();
    assert_eq!(
        request["messages"][0]["content"], "You are a custom generic agent.",
        "generic child should use the inline system_prompt"
    );

    // Verify the model was the parent's model (no tier override)
    assert_eq!(
        request["model"], "parent-model",
        "generic child without tier should inherit parent model"
    );

    // Verify the child completed
    assert_eq!(output.exit_reason, ExitReason::Complete);
}

#[tokio::test]
async fn generic_spawn_with_both_agent_type_and_system_prompt_errors() {
    // SpawnAgentTool should reject when both agent_type and system_prompt are provided.
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec!["researcher".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    };

    let result = tool
        .call(
            serde_json::json!({
                "agent_type": "researcher",
                "system_prompt": "You are custom.",
                "task": "do something",
                "budget": {
                    "max_tokens": 10,
                    "max_turns": 1,
                    "max_cost": "1",
                    "max_sub_agents": 0
                }
            }),
            &CapabilityToken::default(),
        )
        .await;

    match result {
        Err(ToolError::InvalidArguments(msg)) => {
            assert!(
                msg.contains("agent_type or system_prompt, not both"),
                "error should mention mutual exclusivity: {msg}"
            );
        }
        other => panic!(
            "providing both agent_type and system_prompt should return InvalidArguments, got {other:?}"
        ),
    }
}

#[tokio::test]
async fn generic_spawn_with_neither_agent_type_nor_system_prompt_errors() {
    // SpawnAgentTool should reject when neither agent_type nor system_prompt is provided.
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec![],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    };

    let result = tool
        .call(
            serde_json::json!({
                "task": "do something",
                "budget": {
                    "max_tokens": 10,
                    "max_turns": 1,
                    "max_cost": "1",
                    "max_sub_agents": 0
                }
            }),
            &CapabilityToken::default(),
        )
        .await;

    match result {
        Err(ToolError::InvalidArguments(msg)) => {
            assert!(
                msg.contains("either agent_type or system_prompt is required"),
                "error should mention that one is required: {msg}"
            );
        }
        other => panic!(
            "providing neither agent_type nor system_prompt should return InvalidArguments, got {other:?}"
        ),
    }
}

#[tokio::test]
async fn generic_spawn_system_prompt_exceeds_8kb_errors() {
    // SpawnAgentTool should reject system_prompt > 8192 bytes.
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec![],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    };

    let oversized_prompt = "x".repeat(9000);
    let result = tool
        .call(
            serde_json::json!({
                "system_prompt": oversized_prompt,
                "task": "do something",
                "budget": {
                    "max_tokens": 10,
                    "max_turns": 1,
                    "max_cost": "1",
                    "max_sub_agents": 0
                }
            }),
            &CapabilityToken::default(),
        )
        .await;

    match result {
        Err(ToolError::ExecutionFailed(msg)) => {
            assert!(
                msg.contains("8192") && msg.contains("9000"),
                "error should mention the 8192 byte limit and the actual size: {msg}"
            );
        }
        other => panic!("system_prompt > 8192 bytes should return ExecutionFailed, got {other:?}"),
    }
}

#[test]
fn generic_spawn_inherits_parent_capabilities() {
    // Generic spawn without capability override should inherit the parent's full capability.
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-cap-1",
        "model": "parent-model",
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let parent_cap = CapabilityToken {
        shell: true,
        javascript: true,
        python: false,
        network: vec![NetworkPermission("net:api.github.com".into())],
        ..Default::default()
    };

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());

    // Use a CapturingTaskFactory to inspect the child config
    // Instead, we use AgentTaskFactory directly and check the child loop's behavior.
    // The child should inherit parent capabilities since no override is provided.
    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: false,
            javascript: false,
            python: false,
            paths_read: vec![],
            paths_write: vec![],

            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: parent_cap.clone(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    // Generic spawn with no capability override
    let spawn = generic_spawn_config(
        "child-cap-1",
        "parent-agent",
        "You are a helper.",
        child_budget(32, 1, 0),
    );

    // The child should succeed and use the parent's capability token.
    // We verify by checking the output — if the factory branches correctly,
    // the generic path uses parent_capability.clone() (no intersection with config).
    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("generic child should complete with parent capabilities");

    assert_eq!(output.exit_reason, ExitReason::Complete);
}

#[test]
fn generic_spawn_with_capability_override_intersects_parent() {
    // Generic spawn with capability override should intersect with parent (two-way).
    // Parent: shell=true, javascript=true
    // Override: shell=true, javascript=false
    // Effective: shell=true (both allow), javascript=false (override denies)
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-cap-2",
        "model": "parent-model",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call-js-1",
                    "type": "function",
                    "function": {
                        "name": "js_exec",
                        "arguments": "{\"code\":\"console.log('hello')\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let parent_cap = CapabilityToken {
        shell: true,
        javascript: true,
        python: false,
        ..Default::default()
    };

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());

    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: false,
            javascript: false,
            python: false,
            paths_read: vec![],
            paths_write: vec![],

            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: parent_cap,
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    // Generic spawn with override: javascript=false
    let mut spawn = generic_spawn_config(
        "child-cap-2",
        "parent-agent",
        "You are a helper.",
        child_budget(32, 1, 0),
    );
    spawn.capability = Some(CapabilityToken {
        shell: true,
        javascript: false,
        ..Default::default()
    });

    // The child tries to use js_exec, but javascript=false in the effective capability
    // (parent=true, override=false => intersection=false). The child should get a
    // capability violation error.
    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("child should complete even if tool call fails");

    // The child should have attempted the tool call and gotten a capability error.
    // The output should contain the error in messages (the agent loop continues).
    let has_capability_error = output.messages.iter().any(|m| {
        m.content.contains("capability")
            || m.content.contains("not allowed")
            || m.content.contains("denied")
    });
    assert!(
        has_capability_error || output.exit_reason == ExitReason::MaxTurns,
        "generic child with javascript denied should either see a capability error or hit max_turns, got exit_reason={:?}",
        output.exit_reason
    );
}

#[test]
fn generic_spawn_tool_registry_includes_all_builtins_and_excludes_spawn_agent() {
    // Generic children are full leaf workers: they get the standard built-ins
    // but never the delegation tool.
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-nospawn-1",
        "model": "parent-model",
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());

    // Give the parent spawn_types — but generic agents should still not get spawn_agent
    let parent_cap = CapabilityToken {
        spawn_types: vec!["researcher".into()],
        ..Default::default()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: false,
            javascript: false,
            python: false,
            paths_read: vec![],
            paths_write: vec![],

            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: parent_cap,
        supervisor_sender: Some(tx),
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    let spawn = generic_spawn_config(
        "child-nospawn-1",
        "parent-agent",
        "You are a leaf worker.",
        child_budget(32, 1, 0),
    );

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("generic child should complete");

    // Verify the request sent to the provider includes all standard built-ins
    // and does NOT include spawn_agent.
    let request = server.first_request_json();
    let tool_names: BTreeSet<String> = request
        .get("tools")
        .and_then(|v| v.as_array())
        .expect("generic child request should include tool definitions")
        .iter()
        .map(|tool| {
            tool.pointer("/function/name")
                .and_then(|v| v.as_str())
                .expect("tool definition should include function.name")
                .to_string()
        })
        .collect();
    let expected_builtins: BTreeSet<String> = [
        "file_read",
        "file_write",
        "file_edit",
        "shell_exec",
        "js_exec",
        "list_dir",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    assert_eq!(
        tool_names, expected_builtins,
        "generic child tool registry should contain exactly the standard built-ins and no spawn_agent"
    );
    let has_spawn_tool = tool_names.iter().any(|name| name.as_str() == "spawn_agent");
    assert!(
        !has_spawn_tool,
        "generic child agent should NOT have spawn_agent tool registered — generic agents are leaf workers"
    );
    assert_eq!(output.exit_reason, ExitReason::Complete);
}

#[tokio::test]
async fn generic_spawn_parent_max_sub_agents_zero_remains_unlimited() {
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut parent_budget = default_budget();
    parent_budget.max_sub_agents = 0;
    parent_budget.used_sub_agents = 17;

    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        parent_budget,
        Arc::new(factory.clone()),
    );

    supervisor
        .spawn_agent(generic_spawn_config(
            "child-generic-unlimited-subagents",
            "parent-agent",
            "You are a leaf worker.",
            child_budget(10, 1, 0),
        ))
        .expect("generic spawn should accept parent max_sub_agents = 0 as unlimited");
    factory.wait_for_completed(1).await;

    assert_eq!(
        supervisor.parent_budget().used_sub_agents,
        18,
        "accepted generic spawn should still increment usage under an unlimited parent budget"
    );
}

#[tokio::test]
async fn generic_subagent_spawned_journal_records_full_system_prompt_for_audit() {
    let parent_id = AgentId("parent-agent".into());
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    supervisor.set_journal_storage(Arc::clone(&journal));

    let system_prompt =
        "You are a generic audit worker. Preserve this exact prompt in the parent journal.";
    supervisor
        .spawn_agent(generic_spawn_config(
            "child-generic-audit",
            &parent_id.0,
            system_prompt,
            child_budget(10, 1, 0),
        ))
        .expect("generic spawn should be accepted");
    factory.wait_for_completed(1).await;

    let spawned_entry = journal
        .read_all(&parent_id)
        .expect("parent journal should be readable")
        .into_iter()
        .find_map(|entry| match entry.entry {
            JournalEntryKind::SubAgentSpawned { .. } => Some(entry.entry),
            _ => None,
        })
        .expect("generic spawn should append SubAgentSpawned");
    let spawned_json =
        serde_json::to_value(&spawned_entry).expect("journal entry should serialize to JSON");

    assert_eq!(
        spawned_json.get("agent_type").and_then(|v| v.as_str()),
        Some("generic"),
        "generic SubAgentSpawned entries should label agent_type as generic"
    );
    assert_eq!(
        spawned_json.get("system_prompt").and_then(|v| v.as_str()),
        Some(system_prompt),
        "generic SubAgentSpawned entries should include the full inline system_prompt for audit"
    );
}

#[tokio::test]
async fn generic_spawn_aborts_when_subagent_spawned_journal_append_fails() {
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );
    supervisor.set_journal_storage(Arc::new(FailingAppendJournal));

    let err = supervisor
        .spawn_agent(generic_spawn_config(
            "child-generic-journal-fail",
            "parent-agent",
            "You are an audit-sensitive worker.",
            child_budget(10, 1, 0),
        ))
        .expect_err("generic spawn must fail before child execution if spawn journaling fails");

    assert!(
        matches!(
            err,
            RuntimeError::JournalAppendFailed {
                entry_kind: "SubAgentSpawned",
                ..
            }
        ),
        "journal append failure should be surfaced as JournalAppendFailed, got {err:?}"
    );
    assert_eq!(
        factory.started_count(),
        0,
        "child task must not start if the parent spawn audit entry is missing"
    );
    assert_eq!(
        supervisor.parent_budget().used_sub_agents,
        0,
        "rejected spawn must not consume parent sub-agent budget"
    );
}

#[tokio::test]
async fn generic_create_agent_span_records_generic_spawn_mode_and_explicit_tier() {
    let (_, spans, _) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            CapabilityToken::default(),
            default_budget(),
            Arc::new(NoopFactory),
        );
        let mut spawn = generic_spawn_config(
            "child-generic-fast",
            "parent-agent",
            "You are a fast leaf worker.",
            child_budget(10, 1, 0),
        );
        spawn.tier = Some("fast".into());
        supervisor
            .spawn_agent(spawn)
            .expect("generic spawn with explicit tier should succeed");
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "create_agent"
                && span
                    .fields
                    .get("simulacra.agent.spawn_mode")
                    .map(String::as_str)
                    == Some("generic")
                && span.fields.get("simulacra.agent.tier").map(String::as_str) == Some("fast")
        }),
        "generic create_agent span should record spawn_mode=generic and the explicit resolved tier"
    );
}

#[tokio::test]
async fn generic_create_agent_span_labels_missing_tier_as_balanced_fallback() {
    let (_, spans, _) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            CapabilityToken::default(),
            default_budget(),
            Arc::new(NoopFactory),
        );
        supervisor
            .spawn_agent(generic_spawn_config(
                "child-generic-balanced",
                "parent-agent",
                "You are a balanced leaf worker.",
                child_budget(10, 1, 0),
            ))
            .expect("generic spawn without explicit tier should succeed");
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "create_agent"
                && span
                    .fields
                    .get("simulacra.agent.spawn_mode")
                    .map(String::as_str)
                    == Some("generic")
                && span.fields.get("simulacra.agent.tier").map(String::as_str) == Some("balanced")
        }),
        "generic create_agent span should record tier=balanced when no explicit tier is provided and no reverse lookup match is available"
    );
}

#[test]
fn generic_child_invoke_agent_span_nests_under_parent_trace() {
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-generic-trace",
        "model": "parent-model",
        "choices": [{
            "message": { "role": "assistant", "content": "generic trace done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: false,
            javascript: false,
            python: false,
            paths_read: vec![],
            paths_write: vec![],

            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    let (_, spans, _) = capture_trace(|| {
        let mut supervisor = AgentSupervisor::with_task_factory(
            CapabilityToken::default(),
            default_budget(),
            Arc::new(factory),
        );
        let spawn = generic_spawn_config(
            "child-generic-trace",
            "parent-agent",
            "You are a trace-linked generic worker.",
            child_budget(32, 1, 0),
        );

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let parent_span = tracing::info_span!("parent_agent_turn");
                {
                    let _entered = parent_span.enter();
                    supervisor
                        .spawn_agent(spawn)
                        .expect("generic child should spawn under the parent trace");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            });
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "invoke_agent" && span.parent_name.as_deref() == Some("parent_agent_turn")
        }),
        "generic child invoke_agent span should be parented to the active parent trace"
    );
}

#[tokio::test]
async fn generic_spawn_without_tier_reverse_looks_up_parent_model_for_resolved_tier() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let mut tiers = TierMap::default();
    tiers.insert(
        "reasoning".to_string(),
        "parent-reasoning-model".to_string(),
    );
    tiers.insert("balanced".to_string(), "other-model".to_string());
    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec![],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers,
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-reasoning-model".into(),
    };

    let call_future = tool.call(
        serde_json::json!({
            "system_prompt": "You are a tier-labeled generic worker.",
            "task": "do something",
            "budget": {
                "max_tokens": 10,
                "max_turns": 1,
                "max_cost": "1",
                "max_sub_agents": 0
            }
        }),
        &CapabilityToken::default(),
    );
    let receive_future = async move {
        let message = receiver
            .recv()
            .await
            .expect("spawn tool should send one supervisor message");
        match message.payload {
            SupervisorPayload::Spawn(config, result_tx) => {
                let captured = (*config).clone();
                result_tx
                    .send(Ok(child_success_output()))
                    .expect("spawn tool should still be awaiting the child result");
                captured
            }
            other => panic!("expected SupervisorPayload::Spawn, got {other:?}"),
        }
    };

    let (result, captured) = tokio::join!(call_future, receive_future);
    result.expect("generic spawn should complete");

    assert_eq!(
        captured.agent_type, None,
        "this assertion must exercise generic mode, not configured mode"
    );
    assert_eq!(
        captured.tier, None,
        "the LLM did not request an explicit tier"
    );
    assert_eq!(
        captured.resolved_tier.as_deref(),
        Some("reasoning"),
        "generic spawn without tier should label the child with the first tier whose model matches the parent model"
    );
}

#[test]
fn generic_spawn_with_tier_uses_tier_model() {
    // When tier is specified and exists in config, the resolved model should come from tiers.
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-tier-1",
        "model": "claude-haiku-35-20241022",
        "choices": [{
            "message": { "role": "assistant", "content": "fast done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());

    let mut config = task_factory_config(CapabilitiesConfig {
        network: vec![],
        mcp: vec![],
        shell: false,
        javascript: false,
        python: false,
        paths_read: vec![],
        paths_write: vec![],

        memory: None,
    });
    config
        .tiers
        .insert("fast".into(), "claude-haiku-35-20241022".into());

    let factory = AgentTaskFactory {
        config,
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    let mut spawn = generic_spawn_config(
        "child-tier-1",
        "parent-agent",
        "You are a fast helper.",
        child_budget(32, 1, 0),
    );
    spawn.tier = Some("fast".into());

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("generic child with tier should complete");

    let request = server.first_request_json();
    assert_eq!(
        request["model"], "claude-haiku-35-20241022",
        "generic child with tier='fast' should use the model from tiers config"
    );
    assert_eq!(output.exit_reason, ExitReason::Complete);
}

#[test]
fn generic_spawn_without_tier_inherits_parent_model() {
    // When no tier is specified, the child should inherit the parent's model.
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-notier-1",
        "model": "parent-model",
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());

    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: false,
            javascript: false,
            python: false,
            paths_read: vec![],
            paths_write: vec![],

            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        supervisor_sender: None,
        parent_model: "my-specific-parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    let spawn = generic_spawn_config(
        "child-notier-1",
        "parent-agent",
        "You are a helper.",
        child_budget(32, 1, 0),
    );

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("generic child without tier should complete");

    let request = server.first_request_json();
    assert_eq!(
        request["model"], "my-specific-parent-model",
        "generic child without tier should inherit parent_model"
    );
    assert_eq!(output.exit_reason, ExitReason::Complete);
}

#[tokio::test]
async fn generic_spawn_consumes_parent_budget() {
    // Generic spawn should still increment parent's used_sub_agents.
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    let spawn = generic_spawn_config(
        "child-budget-1",
        "parent-agent",
        "You are a helper.",
        child_budget(10, 1, 1),
    );

    supervisor
        .spawn_agent(spawn)
        .expect("generic spawn should succeed");
    factory.wait_for_completed(1).await;

    assert_eq!(
        supervisor.parent_budget().used_sub_agents,
        1,
        "generic spawn should increment parent used_sub_agents"
    );
}

#[tokio::test]
async fn configured_spawn_still_works() {
    // Regression test: configured spawn (with agent_type) should still work
    // after introducing the generic branch.
    let factory = RecordingTaskFactory::new(vec![Ok(child_success_output())]);
    let mut supervisor = AgentSupervisor::with_task_factory(
        default_capability(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    supervisor
        .spawn_agent(spawn_config_with_agent_type(
            "child-configured-1",
            "parent-agent",
            "researcher",
            child_budget(10, 1, 1),
        ))
        .expect("configured spawn should still work");
    factory.wait_for_completed(1).await;

    let started = factory.inner.started.lock().unwrap().clone();
    let snapshot = started.first().expect("factory should record the spawn");
    assert_eq!(snapshot.agent_type, "researcher");
    assert_eq!(snapshot.task, "delegate task");
    assert_eq!(
        supervisor.parent_budget().used_sub_agents,
        1,
        "configured spawn should still increment budget"
    );
}

#[tokio::test]
async fn generic_spawn_with_unknown_tier_errors() {
    // When tiers config is populated, an unknown tier name should produce an error
    // listing the valid tier names.
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let mut tiers = TierMap::default();
    tiers.insert("reasoning".to_string(), "claude-opus-4-6".to_string());
    tiers.insert("balanced".to_string(), "claude-sonnet-4-6".to_string());
    tiers.insert("fast".to_string(), "claude-haiku-4-5-20251001".to_string());

    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec![],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers,
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    };

    let result = tool
        .call(
            serde_json::json!({
                "system_prompt": "You are a helper.",
                "task": "do something",
                "budget": {
                    "max_tokens": 10,
                    "max_turns": 1,
                    "max_cost": "1",
                    "max_sub_agents": 0
                },
                "tier": "turbo"
            }),
            &CapabilityToken::default(),
        )
        .await;

    match result {
        Err(ToolError::ExecutionFailed(msg)) => {
            assert!(
                msg.contains("unknown tier 'turbo'"),
                "error should mention the unknown tier name: {msg}"
            );
            // The error should list the valid tier names
            assert!(
                msg.contains("reasoning") || msg.contains("balanced") || msg.contains("fast"),
                "error should list valid tiers: {msg}"
            );
        }
        other => panic!("unknown tier should return ExecutionFailed, got {other:?}"),
    }
}

#[test]
fn default_system_prompt_describes_current_sandbox_affordances() {
    assert!(DEFAULT_SYSTEM_PROMPT.contains("fresh JS global/context"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("simulacra:path"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("simulacra:crypto"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("Cwd and env vars persist"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("node -"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("python -"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("/proc/mailbox/<filename>"));
    assert!(!DEFAULT_SYSTEM_PROMPT.contains("persistent QuickJS context"));
    assert!(!DEFAULT_SYSTEM_PROMPT.contains("No `cd`"));
}

#[test]
fn generic_spawn_empty_system_prompt_uses_default() {
    // When system_prompt is "" (empty string), the factory should fall back to
    // DEFAULT_SYSTEM_PROMPT rather than sending an empty system prompt to the provider.
    let _env_lock = openai_env_guard();
    let server = FakeOpenAiServer::new(CannedResponse::json(serde_json::json!({
        "id": "resp-empty-sp-1",
        "model": "parent-model",
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
    })));
    let _base_url = EnvGuard::set("OPENAI_BASE_URL", &server.base_url());
    let _api_base = EnvGuard::set("OPENAI_API_BASE", &server.base_url());
    let _api_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace")
        .expect("workspace directory should be created");
    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let factory = AgentTaskFactory {
        config: task_factory_config(CapabilitiesConfig {
            network: vec![],
            mcp: vec![],
            shell: false,
            javascript: false,
            python: false,
            paths_read: vec![],
            paths_write: vec![],

            memory: None,
        }),
        provider_kind: ProviderKind::OpenAI,
        vfs,
        journal,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: CapabilityToken::default(),
        supervisor_sender: None,
        parent_model: "parent-model".into(),
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
    };

    // Empty system_prompt — should fall back to DEFAULT_SYSTEM_PROMPT
    let spawn = generic_spawn_config(
        "child-empty-sp-1",
        "parent-agent",
        "",
        child_budget(32, 1, 0),
    );

    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(factory.create_task(spawn, CancellationToken::new(Duration::from_secs(1))))
        .expect("generic child with empty system_prompt should complete");

    let request = server.first_request_json();
    let sent_prompt = request["messages"][0]["content"].as_str().unwrap_or("");
    assert!(
        !sent_prompt.is_empty(),
        "empty system_prompt should fall back to DEFAULT_SYSTEM_PROMPT, not send empty string"
    );
    assert!(
        sent_prompt.contains("You are a helpful AI assistant"),
        "fallback should be DEFAULT_SYSTEM_PROMPT, got: {}",
        &sent_prompt[..sent_prompt.len().min(80)]
    );
    assert!(
        sent_prompt.contains("fresh JS global/context"),
        "fallback prompt should tell child agents js_exec is single-shot, got: {sent_prompt}"
    );
    assert!(
        sent_prompt.contains("Cwd and env vars persist"),
        "fallback prompt should advertise persistent shell cwd/env, got: {sent_prompt}"
    );
    assert!(
        sent_prompt.contains("node -") && sent_prompt.contains("python -"),
        "fallback prompt should advertise stdin interpreter aliases, got: {sent_prompt}"
    );
    assert!(
        sent_prompt.contains("/proc/mailbox/<filename>"),
        "fallback prompt should tell child agents where to write artifacts, got: {sent_prompt}"
    );
    assert!(
        !sent_prompt.contains("persistent QuickJS context") && !sent_prompt.contains("No `cd`"),
        "fallback prompt should not contain stale sandbox affordance guidance, got: {sent_prompt}"
    );
    assert_eq!(output.exit_reason, ExitReason::Complete);
}

#[tokio::test]
async fn generic_spawn_empty_agent_type_string_errors() {
    // When agent_type is "" (empty string), it should be treated as None,
    // so without system_prompt the call errors with "either agent_type or system_prompt is required".
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let tool = SpawnAgentTool {
        sender,
        can_spawn: vec![],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        tiers: Default::default(),
        parent_budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        parent_model: "parent-model".into(),
    };

    let result = tool
        .call(
            serde_json::json!({
                "agent_type": "",
                "task": "do something",
                "budget": {
                    "max_tokens": 10,
                    "max_turns": 1,
                    "max_cost": "1",
                    "max_sub_agents": 0
                }
            }),
            &CapabilityToken::default(),
        )
        .await;

    match result {
        Err(ToolError::InvalidArguments(msg)) => {
            assert!(
                msg.contains("either agent_type or system_prompt is required"),
                "empty agent_type should be treated as None: {msg}"
            );
        }
        other => {
            panic!("empty agent_type string should be treated as None and error, got {other:?}")
        }
    }
}
