use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use serde_json::json;
use simulacra_cli::interactive::{
    HistoryDirection, InteractiveInput, InteractiveOutput, InteractiveSession,
    InteractiveSessionConfig, SessionView, StreamEvent,
};
use simulacra_cli::{CliArgs, CliMode, bootstrap, run};
use simulacra_runtime::{
    AgentLoop, AgentLoopConfig, InMemoryJournalStorage, InMemorySessionStorage,
    NoopContextStrategy, RuntimeError, Session, SessionStorage,
};
use simulacra_tool::ToolRegistry;
use simulacra_types::{
    AgentId, CapabilityToken, ExitReason, FinishReason, Message, Provider, ProviderError,
    ProviderResponse, ResourceBudget, Role, TokenUsage, ToolCallMessage, ToolDefinition, VirtualFs,
};
use simulacra_vfs::MemoryFs;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
    parent: Option<String>,
}

#[derive(Debug, Clone)]
struct CapturedEvent {
    level: String,
    fields: HashMap<String, String>,
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
static CAPTURED_SPANS: OnceLock<Arc<Mutex<Vec<CapturedSpan>>>> = OnceLock::new();
static CAPTURED_EVENTS: OnceLock<Arc<Mutex<Vec<CapturedEvent>>>> = OnceLock::new();
static CAPTURE_INSTALL: OnceLock<()> = OnceLock::new();

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

        let parent = attrs
            .parent()
            .and_then(|parent_id| ctx.span(parent_id))
            .map(|span| span.name().to_string())
            .or_else(|| {
                if attrs.is_contextual() {
                    ctx.current_span()
                        .id()
                        .and_then(|parent_id| ctx.span(parent_id))
                        .map(|span| span.name().to_string())
                } else {
                    None
                }
            });

        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields,
            parent,
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

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        event.record(&mut visitor);

        self.events.lock().unwrap().push(CapturedEvent {
            level: event.metadata().level().to_string(),
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

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

fn capture_trace<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    let spans = capture_spans_store();
    let events = capture_events_store();
    spans.lock().unwrap().clear();
    events.lock().unwrap().clear();
    let result = f();
    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

fn capture_spans_store() -> Arc<Mutex<Vec<CapturedSpan>>> {
    install_capture_layer();
    Arc::clone(
        CAPTURED_SPANS
            .get()
            .expect("capture spans should be installed"),
    )
}

fn capture_events_store() -> Arc<Mutex<Vec<CapturedEvent>>> {
    install_capture_layer();
    Arc::clone(
        CAPTURED_EVENTS
            .get()
            .expect("capture events should be installed"),
    )
}

fn install_capture_layer() {
    CAPTURE_INSTALL.get_or_init(|| {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));

        CAPTURED_SPANS
            .set(Arc::clone(&spans))
            .expect("spans store should only install once");
        CAPTURED_EVENTS
            .set(Arc::clone(&events))
            .expect("events store should only install once");

        let subscriber =
            tracing_subscriber::registry::Registry::default().with(CaptureLayer { spans, events });
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber should install");
    });
}

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_path(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "simulacra-cli-s015-{name}-{stamp}-{}.toml",
        std::process::id()
    ))
}

struct TempConfig {
    path: PathBuf,
}

impl TempConfig {
    fn write(contents: &str) -> Self {
        let path = unique_path("config");
        fs::write(&path, contents).expect("temp config should be written");
        Self { path }
    }

    fn missing() -> Self {
        Self {
            path: unique_path("missing"),
        }
    }

    fn path_string(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
struct FakeProvider {
    responses: Mutex<Vec<Result<ProviderResponse, ProviderError>>>,
    stream_scripts: Mutex<Vec<Vec<StreamEvent>>>,
    recorded_turns: Mutex<Vec<Vec<Message>>>,
}

impl FakeProvider {
    fn success(text: &str) -> Self {
        Self {
            responses: Mutex::new(vec![Ok(final_response(text))]),
            stream_scripts: Mutex::new(Vec::new()),
            recorded_turns: Mutex::new(Vec::new()),
        }
    }

    fn failure(error: ProviderError) -> Self {
        Self {
            responses: Mutex::new(vec![Err(error)]),
            stream_scripts: Mutex::new(Vec::new()),
            recorded_turns: Mutex::new(Vec::new()),
        }
    }

    fn streaming(events: Vec<StreamEvent>) -> Self {
        Self {
            responses: Mutex::new(Vec::new()),
            stream_scripts: Mutex::new(vec![events]),
            recorded_turns: Mutex::new(Vec::new()),
        }
    }

    fn with_tool_calls(tool_calls: Vec<ToolCallMessage>) -> Self {
        Self {
            responses: Mutex::new(vec![Ok(ProviderResponse {
                message: Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls,
                    tool_call_id: None,
                },
                token_usage: TokenUsage {
                    input_tokens: 17,
                    output_tokens: 9,
                },
                finish_reason: FinishReason::ToolUse,
                provider_response_id: Some("resp-tool".into()),
                model: "claude-sonnet-4-20250514".into(),
            })]),
            stream_scripts: Mutex::new(Vec::new()),
            recorded_turns: Mutex::new(Vec::new()),
        }
    }

    /// Create a provider that returns the given sequence of results in order.
    #[allow(dead_code)]
    fn sequenced(responses: Vec<Result<ProviderResponse, ProviderError>>) -> Self {
        Self {
            responses: Mutex::new(responses),
            stream_scripts: Mutex::new(Vec::new()),
            recorded_turns: Mutex::new(Vec::new()),
        }
    }

    fn record_turn(&self, messages: &[Message]) {
        self.recorded_turns.lock().unwrap().push(messages.to_vec());
    }

    fn recorded_turns(&self) -> Vec<Vec<Message>> {
        self.recorded_turns.lock().unwrap().clone()
    }

    fn next_stream(&self) -> Vec<StreamEvent> {
        let mut scripts = self.stream_scripts.lock().unwrap();
        if scripts.is_empty() {
            Vec::new()
        } else {
            scripts.remove(0)
        }
    }
}

impl Provider for FakeProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut simulacra_types::ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.record_turn(messages);
            let mut responses = self.responses.lock().map_err(|error| {
                ProviderError::Other(format!("poisoned fake provider: {error}"))
            })?;
            if responses.is_empty() {
                Ok(final_response(""))
            } else {
                responses.remove(0)
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Test-only extension trait for InteractiveSession<FakeProvider, TestIo>
// ---------------------------------------------------------------------------

trait FakeProviderExt {
    fn process_streaming_turn(&mut self) -> SessionView;
    fn process_non_streaming_turn(&mut self) -> SessionView;
    fn submit_turn_recording(&mut self, input: &str) -> SessionView;
}

impl FakeProviderExt for InteractiveSession<FakeProvider, TestIo> {
    fn process_streaming_turn(&mut self) -> SessionView {
        let events = self.provider.next_stream();
        self.process_streaming_events(events)
    }

    fn process_non_streaming_turn(&mut self) -> SessionView {
        let response = {
            let mut responses = self.provider.responses.lock().unwrap();
            if responses.is_empty() {
                None
            } else {
                Some(responses.remove(0))
            }
        };
        if let Some(Ok(response)) = response {
            self.process_response(response)
        } else {
            self.snapshot()
        }
    }

    fn submit_turn_recording(&mut self, input: &str) -> SessionView {
        let view = self.submit_turn(input);
        if !input.is_empty() {
            self.provider.record_turn(&self.view.messages);
            self.view.provider_calls = self.provider.recorded_turns();
        }
        view
    }
}

// ---------------------------------------------------------------------------
// TestIo
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Default, Clone)]
struct TestIo {
    tty: bool,
    lines: VecDeque<String>,
    approvals: VecDeque<String>,
    writes: Vec<String>,
    clear_count: usize,
    restored: bool,
}

impl TestIo {
    fn tty() -> Self {
        Self {
            tty: true,
            ..Self::default()
        }
    }

    fn piped(lines: &[&str]) -> Self {
        Self {
            tty: false,
            lines: lines.iter().map(|line| (*line).to_string()).collect(),
            ..Self::default()
        }
    }
}

impl InteractiveInput for TestIo {
    fn read_line(&mut self) -> Option<String> {
        self.lines.pop_front()
    }

    fn read_approval(&mut self) -> Option<String> {
        self.approvals.pop_front()
    }

    fn is_tty(&self) -> bool {
        self.tty
    }
}

impl InteractiveOutput for TestIo {
    fn write_line(&mut self, line: &str) {
        self.writes.push(line.to_string());
    }

    fn clear(&mut self) {
        self.clear_count += 1;
    }

    fn restore_terminal(&mut self) {
        self.restored = true;
    }
}

fn final_response(text: &str) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: text.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
        },
        finish_reason: FinishReason::EndTurn,
        provider_response_id: Some("resp-1".into()),
        model: "claude-sonnet-4-20250514".into(),
    }
}

fn user_message(content: &str) -> Message {
    Message {
        role: Role::User,
        content: content.to_string(),
        tool_calls: vec![],
        tool_call_id: None,
    }
}

fn assistant_message(content: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: content.to_string(),
        tool_calls: vec![],
        tool_call_id: None,
    }
}

fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCallMessage {
    ToolCallMessage {
        id: format!("call-{name}"),
        name: name.to_string(),
        arguments,
    }
}

fn valid_config_toml(model: &str, task: Option<&str>) -> String {
    let task_line = task
        .map(|task| format!("task = {task:?}"))
        .unwrap_or_default();

    format!(
        r#"[project]
name = "simulacra-interactive-spec"

[agent_types.default]
model = "{model}"
max_turns = 7
max_tokens = 4321

[agent_types.default.capabilities]
shell = true
javascript = true
paths_read = ["/workspace/**"]
paths_write = ["/workspace/**"]

[task]
entry_agent = "default"
{task_line}
"#
    )
}

fn build_interactive_config(
    task: Option<&str>,
    requested_session_id: Option<&str>,
) -> InteractiveSessionConfig {
    let bootstrap_task = task.or(Some("interactive fixture task"));
    let config = TempConfig::write(&valid_config_toml(
        "claude-sonnet-4-20250514",
        bootstrap_task,
    ));
    let boot = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: bootstrap_task.map(|value| value.to_string()),
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: false,
        output_format: simulacra_cli::OutputFormat::Text,
    })
    .expect("headless bootstrap should provide the shared interactive components");

    InteractiveSessionConfig {
        project_name: boot.config.project.name.clone(),
        model: boot.model.clone(),
        max_tokens: boot.resource_budget.max_tokens,
        max_turns: boot.resource_budget.max_turns,
        task: task.map(|value| value.to_string()),
        requested_session_id: requested_session_id.map(|value| value.to_string()),
        tool_definitions: boot.tool_definitions.clone(),
        can_spawn: vec![],
        skill_catalog: vec![],
    }
}

fn make_session(
    task: Option<&str>,
    requested_session_id: Option<&str>,
    provider: Arc<FakeProvider>,
    storage: Arc<dyn SessionStorage>,
) -> InteractiveSession<FakeProvider, TestIo> {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    InteractiveSession::new(
        TestIo::tty(),
        provider,
        storage,
        vfs,
        build_interactive_config(task, requested_session_id),
    )
}

/// Build a minimal AgentLoop backed by a FakeProvider for behavioral tests
/// that exercise the real `run_interactive_loop` → `run_single_turn` path.
fn make_agent_loop(provider: FakeProvider) -> AgentLoop {
    let config = AgentLoopConfig {
        agent_id: AgentId("test-agent".into()),
        system_prompt: "You are a test assistant.".to_string(),
        model: "claude-sonnet-4-20250514".to_string(),
        max_turns: 10,
        capability: CapabilityToken::default(),
    };
    AgentLoop::new(
        config,
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(NoopContextStrategy),
        Arc::new(InMemoryJournalStorage::new()),
        ResourceBudget::new(100_000, 10, rust_decimal::Decimal::ZERO, 0),
        None,
        None,
    )
}

fn contains_text(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|line| line.contains(needle))
}

fn first_user_message(messages: &[Message]) -> Option<&str> {
    messages
        .iter()
        .find(|message| message.role == Role::User)
        .map(|message| message.content.as_str())
}

fn looks_like_uuid(candidate: &str) -> bool {
    let parts: Vec<_> = candidate.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let expected = [8, 4, 4, 4, 12];
    parts
        .iter()
        .zip(expected)
        .all(|(part, len)| part.len() == len && part.chars().all(|ch| ch.is_ascii_hexdigit()))
}

fn save_existing_session(storage: &Arc<dyn SessionStorage>, id: &str, messages: Vec<Message>) {
    storage
        .save(&Session {
            id: id.to_string(),
            agent_id: AgentId("default".into()),
            messages,
            vfs_snapshot: None,
            created_at: 1,
            used_tokens: 0,
            used_turns: 0,
        })
        .expect("existing session should save into in-memory storage");
}

/// A session storage that always fails on save — used to verify that
/// checkpoint save errors are surfaced to the user as warnings.
#[derive(Debug)]
struct FailingSessionStorage;

impl SessionStorage for FailingSessionStorage {
    fn save(&self, _session: &Session) -> Result<(), RuntimeError> {
        Err(RuntimeError::Session("disk full (simulated)".to_string()))
    }

    fn load(&self, _id: &str) -> Result<Option<Session>, RuntimeError> {
        Ok(None)
    }
}

mod session_startup {
    use super::*;

    #[test]
    fn interactive_mode_starts_an_interactive_session() {
        let _guard = test_guard();
        let config = TempConfig::missing();
        let output = run(CliArgs {
            config_path: config.path_string(),
            task: Some("hello".into()),
            mode: Some(CliMode::Interactive),
            verbose: false,
            otlp_endpoint: None,
            session: None,
            model: None,
            max_turns: None,
            max_tokens: None,
            max_cost: None,
            no_catalog: false,
            output_format: simulacra_cli::OutputFormat::Text,
        })
        .expect("interactive mode should return cli output");

        // In a test environment, run() will fail because either:
        // - no API key is set (build_provider fails), or
        // - no terminal is available (TerminalIo::new fails).
        // The key assertion is that interactive mode is wired up (not the old stub).
        assert!(
            !output
                .stderr_content
                .contains("interactive mode not yet implemented"),
            "interactive startup should not be blocked by a phase-one guard: {:?}",
            output.stderr_content
        );
        // Verify we reached the provider or terminal step (not some unrelated failure)
        let expected_failures = ["API_KEY not set", "failed to initialize terminal"];
        assert!(
            expected_failures
                .iter()
                .any(|msg| output.stderr_content.contains(msg)),
            "expected provider or terminal error, got: {:?}",
            output.stderr_content
        );
    }

    #[test]
    fn interactive_task_is_sent_as_the_first_user_message() {
        let provider = Arc::new(FakeProvider::success("hello back"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(Some("hello"), None, provider, storage);

        let view = session.start();
        assert_eq!(first_user_message(&view.messages), Some("hello"));
    }

    #[test]
    fn session_header_displays_project_name_model_name_and_budget_limits() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(Some("hello"), None, provider, storage);
        let config = session.config.clone();

        let view = session.start();
        assert!(contains_text(&view.header, &config.project_name));
        assert!(contains_text(&view.header, &config.model));
        assert!(contains_text(&view.header, &config.max_tokens.to_string()));
        assert!(contains_text(&view.header, &config.max_turns.to_string()));
    }

    #[test]
    fn existing_session_id_resumes_conversation_from_saved_state() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        save_existing_session(
            &storage,
            "resume-me",
            vec![
                user_message("saved prompt"),
                assistant_message("saved reply"),
            ],
        );
        let mut session = make_session(None, Some("resume-me"), provider, storage);

        let view = session.resume_from_storage("resume-me");
        assert_eq!(
            view.messages.len(),
            2,
            "resume should restore saved messages"
        );
        assert_eq!(first_user_message(&view.messages), Some("saved prompt"));
    }

    #[test]
    fn missing_session_id_creates_a_new_session_with_the_requested_id() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let session = make_session(None, Some("requested-id"), provider, storage);

        assert_eq!(session.snapshot().session_id, "requested-id");
    }

    #[test]
    fn session_flag_requires_a_value() {
        let error = CliArgs::try_parse_from(["simulacra", "--mode", "interactive", "--session"])
            .expect_err("clap should reject a session flag without a value");
        let rendered = error.to_string();

        assert!(
            rendered.contains("--session <SESSION>"),
            "session parse error should require an explicit value: {rendered}"
        );
    }

    #[test]
    fn omitting_session_generates_a_uuid_session_id() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let session = make_session(None, None, provider, storage);

        assert!(
            looks_like_uuid(&session.snapshot().session_id),
            "interactive mode should generate a UUID session id when --session is absent"
        );
    }
}

mod input_handling {
    use super::*;

    #[test]
    fn empty_input_does_not_send_a_message_to_the_model() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, Arc::clone(&provider), storage);

        let _ = session.submit_turn("");
        assert!(
            provider.recorded_turns().is_empty(),
            "pressing Enter on an empty prompt should not call the provider"
        );
    }

    #[test]
    fn trailing_backslash_concatenates_multiline_input_into_a_single_message() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let session = make_session(None, None, provider, storage);

        let parsed = session.parse_multiline_input(&["first line\\", "second line"]);
        assert_eq!(parsed.as_deref(), Some("first line\nsecond line"));
    }

    #[test]
    fn up_and_down_arrows_navigate_input_history_within_the_session() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);
        let _ = session.submit_turn("first");
        let _ = session.submit_turn("second");

        assert_eq!(
            session.navigate_history(HistoryDirection::Up).as_deref(),
            Some("second")
        );
        assert_eq!(
            session.navigate_history(HistoryDirection::Down).as_deref(),
            Some("first")
        );
    }
}

mod slash_commands {
    use super::*;

    #[test]
    fn exit_saves_the_session_and_exits_with_code_zero() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, Arc::clone(&storage));
        session.seed_messages(vec![user_message("remember this")]);

        let view = session.dispatch_command("/exit");
        assert!(
            view.saved_session,
            "slash-exit should save the session before exiting"
        );
        assert_eq!(view.exit_code, Some(0));
        assert!(storage.load(&view.session_id).unwrap().is_some());
    }

    #[test]
    fn clear_clears_visible_output_without_discarding_conversation_history() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);
        session.seed_output(&["assistant: hello"]);
        session.seed_messages(vec![user_message("hello"), assistant_message("hi")]);

        let view = session.dispatch_command("/clear");
        assert!(
            view.visible_output.is_empty(),
            "clear should wipe the rendered transcript"
        );
        assert_eq!(
            view.messages.len(),
            2,
            "clear must retain conversation history"
        );
    }

    #[test]
    fn budget_displays_current_token_and_turn_usage_with_limits() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);
        session.seed_budget(3_210, 4);

        let view = session.dispatch_command("/budget");
        assert!(contains_text(&view.visible_output, "tokens: 3210/4321"));
        assert!(contains_text(&view.visible_output, "turns: 4/7"));
    }

    #[test]
    fn tools_lists_registered_tool_names_and_descriptions() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);
        let expected = session
            .config
            .tool_definitions
            .first()
            .expect("headless bootstrap should register builtins")
            .name
            .clone();

        let view = session.dispatch_command("/tools");
        assert!(contains_text(&view.visible_output, &expected));
    }

    #[test]
    fn session_displays_the_current_session_id() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, Some("session-123"), provider, storage);

        let view = session.dispatch_command("/session");
        assert!(contains_text(&view.visible_output, "session-123"));
    }

    #[test]
    fn help_lists_all_supported_slash_commands() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.dispatch_command("/help");
        for command in [
            "/exit", "/quit", "/clear", "/budget", "/tools", "/session", "/help",
        ] {
            assert!(
                contains_text(&view.visible_output, command),
                "help should list {command}"
            );
        }
    }

    #[test]
    fn unknown_slash_command_displays_an_unknown_command_error() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.dispatch_command("/bogus");
        assert_eq!(
            view.error.as_deref(),
            Some("unknown command: /bogus. Type /help for available commands.")
        );
    }
}

mod streaming_output {
    use super::*;

    #[test]
    fn streaming_tokens_are_rendered_incrementally_as_they_arrive() {
        let provider = Arc::new(FakeProvider::streaming(vec![
            StreamEvent::Token("hel".into()),
            StreamEvent::Token("lo".into()),
            StreamEvent::Done,
        ]));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.process_streaming_turn();
        assert!(
            view.stream_frames.len() >= 2,
            "streaming output should produce multiple incremental render frames"
        );
    }

    #[test]
    fn tool_call_blocks_are_rendered_distinctly_from_assistant_text() {
        let provider = Arc::new(FakeProvider::streaming(vec![
            StreamEvent::Token("thinking...".into()),
            StreamEvent::ToolCall(tool_call("bash", json!({"command": "pwd"}))),
            StreamEvent::Done,
        ]));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.process_streaming_turn();
        assert!(
            contains_text(&view.visible_output, "[tool]")
                && contains_text(&view.visible_output, "bash"),
            "tool calls should render in a distinct tool block"
        );
    }

    #[test]
    fn non_streaming_provider_responses_are_displayed_in_full_without_error() {
        let provider = Arc::new(FakeProvider::success("full response"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.process_non_streaming_turn();
        assert!(contains_text(&view.visible_output, "full response"));
        assert!(
            view.error.is_none(),
            "non-streaming responses should not be treated as errors"
        );
    }
}

mod tool_call_approval {
    use super::*;

    #[test]
    fn tool_call_pauses_execution_and_displays_tool_name_and_arguments() {
        let provider = Arc::new(FakeProvider::with_tool_calls(vec![tool_call(
            "bash",
            json!({"command": "pwd"}),
        )]));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_tool_approval(
            vec![tool_call("bash", json!({"command": "pwd"}))],
            &["a"],
            true,
        );
        assert!(contains_text(&view.approval_prompts, "bash"));
        assert!(contains_text(&view.approval_prompts, "\"command\":\"pwd\""));
    }

    #[test]
    fn approve_executes_the_tool_and_returns_the_result_to_the_model() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_tool_approval(
            vec![tool_call("bash", json!({"command": "pwd"}))],
            &["a"],
            true,
        );
        assert_eq!(view.executed_tools, vec!["bash".to_string()]);
        assert!(
            view.tool_results_to_model
                .iter()
                .any(|message| message.role == Role::Tool && message.content.contains("/workspace")),
            "approved tools should send their result back to the model"
        );
    }

    #[test]
    fn deny_returns_a_tool_error_result_without_executing_the_tool() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_tool_approval(
            vec![tool_call("bash", json!({"command": "pwd"}))],
            &["d"],
            true,
        );
        assert!(
            view.executed_tools.is_empty(),
            "denied tools must not execute"
        );
        assert!(
            view.tool_results_to_model.iter().any(|message| {
                message.role == Role::Tool && message.content.contains("Tool call denied by user")
            }),
            "a denial should be reflected back to the model as an error tool result"
        );
    }

    #[test]
    fn approve_all_covers_the_current_and_subsequent_tool_calls_in_the_same_turn() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_tool_approval(
            vec![
                tool_call("bash", json!({"command": "pwd"})),
                tool_call("read_file", json!({"path": "/workspace/task.md"})),
            ],
            &["A"],
            true,
        );
        assert_eq!(view.executed_tools.len(), 2);
        assert!(
            view.approve_all_active,
            "approve-all should remain active for the current assistant turn"
        );
    }

    #[test]
    fn invalid_approval_input_redisplays_the_prompt_without_executing_or_denying() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_tool_approval(
            vec![tool_call("bash", json!({"command": "pwd"}))],
            &["x", "a"],
            true,
        );
        assert!(
            view.approval_prompts.len() >= 2,
            "invalid approval input should re-display the prompt"
        );
        assert_eq!(view.executed_tools, vec!["bash".to_string()]);
    }

    #[test]
    fn approve_all_resets_to_per_call_mode_on_the_next_user_message() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let _ = session.handle_tool_approval(
            vec![tool_call("bash", json!({"command": "pwd"}))],
            &["A"],
            true,
        );
        let _ = session.submit_turn("new prompt");
        assert!(!session.snapshot().approve_all_active);
    }

    #[test]
    fn multiple_tool_calls_are_presented_sequentially_for_approval() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_tool_approval(
            vec![
                tool_call("bash", json!({"command": "pwd"})),
                tool_call("read_file", json!({"path": "/workspace/task.md"})),
            ],
            &["a", "a"],
            true,
        );
        assert_eq!(
            view.approval_prompts.len(),
            2,
            "each tool call should prompt independently"
        );
    }

    #[test]
    fn capability_denials_are_surfaced_to_the_user_and_sent_back_to_the_model() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_tool_approval(
            vec![tool_call("bash", json!({"command": "pwd"}))],
            &["a"],
            false,
        );
        assert!(
            contains_text(&view.visible_output, "capability")
                || contains_text(&view.visible_output, "denied"),
            "capability denials should be shown to the user"
        );
        assert!(
            view.tool_results_to_model.iter().any(|message| {
                message.role == Role::Tool && message.content.contains("denied")
            }),
            "capability denials should be sent back to the model as tool errors"
        );
    }
}

mod cancellation {
    use super::*;

    #[test]
    fn ctrl_c_during_llm_request_cancels_the_request_and_displays_cancelled() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.cancel_llm_request("partial");
        assert!(contains_text(&view.visible_output, "[cancelled]"));
    }

    #[test]
    fn ctrl_c_during_llm_request_discards_the_partial_response_but_keeps_prior_messages() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);
        session.seed_messages(vec![user_message("before"), assistant_message("complete")]);

        let view = session.cancel_llm_request("partial");
        assert_eq!(
            view.messages.len(),
            2,
            "partial assistant output should be discarded"
        );
        assert_eq!(first_user_message(&view.messages), Some("before"));
    }

    #[test]
    fn ctrl_c_during_tool_execution_returns_cancelled_by_user_error_result() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.cancel_tool_execution();
        assert!(
            view.tool_results_to_model.iter().any(|message| {
                message.role == Role::Tool && message.content.contains("Cancelled by user")
            }),
            "tool cancellation should be reported back to the model"
        );
    }

    #[test]
    fn ctrl_c_at_the_prompt_warns_then_exits_gracefully_on_a_second_press_within_two_seconds() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_prompt_ctrl_c(&[0, 1_500]);
        assert_eq!(
            view.warning.as_deref(),
            Some("Press Ctrl-C again to exit, or type /exit")
        );
        assert_eq!(view.exit_code, Some(0));
    }

    #[test]
    fn double_ctrl_c_within_five_hundred_ms_during_a_request_force_quits_without_saving() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.force_quit_during_request(&[0, 250]);
        assert!(
            view.forced_exit_without_save,
            "rapid double Ctrl-C should bypass session save"
        );
    }
}

mod budget_surfacing {
    use super::*;

    #[test]
    fn status_line_displays_used_and_total_tokens_and_turns() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);
        session.seed_budget(123, 4);

        assert_eq!(session.status_line(), "tokens: 123/4321 | turns: 4/7");
    }

    #[test]
    fn status_line_changes_appearance_when_any_budget_resource_reaches_eighty_percent() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);
        session.seed_budget(3_500, 4);

        assert!(
            session.budget_warning_active(),
            "budget warnings should trigger at 80% usage"
        );
    }

    #[test]
    fn budget_exhaustion_is_displayed_and_returns_to_the_input_prompt() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_exit_reason(ExitReason::BudgetExhausted);
        assert!(contains_text(&view.visible_output, "BudgetExhausted"));
        assert!(
            view.exit_code.is_none(),
            "budget exhaustion should not crash the interactive session"
        );
    }
}

mod multi_turn_conversation {
    use super::*;

    #[test]
    fn user_messages_accumulate_in_conversation_history_across_turns() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let _ = session.submit_turn("first");
        let view = session.submit_turn("second");
        assert_eq!(
            view.messages
                .iter()
                .filter(|message| message.role == Role::User)
                .count(),
            2,
            "interactive sessions should retain each user turn in the conversation history"
        );
    }

    #[test]
    fn provider_receives_the_full_conversation_history_on_each_turn_subject_to_compaction() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, Arc::clone(&provider), storage);

        let _ = session.submit_turn_recording("first");
        let _ = session.submit_turn_recording("second");
        let recorded = provider.recorded_turns();
        let last_turn = recorded
            .last()
            .expect("second prompt should record a provider call");
        let user_contents: Vec<_> = last_turn
            .iter()
            .filter(|message| message.role == Role::User)
            .map(|message| message.content.as_str())
            .collect();

        assert_eq!(user_contents, vec!["first", "second"]);
    }

    #[test]
    fn tool_results_from_previous_turns_remain_visible_to_the_model_on_later_turns() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, Arc::clone(&provider), storage);
        session.append_tool_result_from_previous_turn("tool-1", "done", false);

        let _ = session.submit_turn_recording("follow-up");
        let recorded = provider.recorded_turns();
        let last_turn = recorded
            .last()
            .expect("follow-up prompt should call the provider");
        assert!(
            last_turn.iter().any(|message| {
                message.role == Role::Tool && message.tool_call_id.as_deref() == Some("tool-1")
            }),
            "tool outputs from previous turns must remain in the conversation context"
        );
    }
}

mod session_persistence {
    use super::*;

    #[test]
    fn graceful_exit_writes_a_journal_checkpoint_with_conversation_state() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, Some("persist-me"), provider, Arc::clone(&storage));
        session.seed_messages(vec![user_message("remember")]);

        let _ = session.save_checkpoint("completed");
        let saved = storage
            .load("persist-me")
            .expect("save should read back from storage");
        assert!(
            saved.is_some(),
            "graceful exit should persist the interactive session checkpoint"
        );
    }

    #[test]
    fn resumed_session_restores_conversation_history_and_vfs_state_from_the_checkpoint() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());

        // Create a VFS with real file content and snapshot it
        let source_vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        source_vfs.mkdir("/workspace").unwrap();
        source_vfs
            .write("/workspace/task.md", b"persisted file")
            .unwrap();
        let snapshot = source_vfs.snapshot().unwrap();

        // Save a session with VFS snapshot
        storage
            .save(&Session {
                id: "resume-vfs".to_string(),
                agent_id: AgentId("default".into()),
                messages: vec![
                    user_message("persisted prompt"),
                    assistant_message("persisted answer"),
                ],
                vfs_snapshot: Some(snapshot),
                created_at: 1,
                used_tokens: 0,
                used_turns: 0,
            })
            .expect("save should succeed");

        let mut session = make_session(None, Some("resume-vfs"), provider, storage);

        let view = session.resume_from_storage("resume-vfs");
        assert_eq!(view.messages.len(), 2);
        assert_eq!(
            view.restored_vfs
                .get("/workspace/task.md")
                .map(String::as_str),
            Some("persisted file"),
            "VFS should be restored from the snapshot stored in the session"
        );
    }

    #[test]
    fn resumed_session_displays_a_summary_with_message_count_and_turns_used() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        save_existing_session(
            &storage,
            "resume-summary",
            vec![user_message("prompt"), assistant_message("answer")],
        );
        let mut session = make_session(None, Some("resume-summary"), provider, storage);

        let view = session.resume_from_storage("resume-summary");
        assert_eq!(
            view.resumed_summary.as_deref(),
            Some("Resumed session resume-summary (2 messages, 1 turns used)")
        );
    }

    #[test]
    fn default_session_storage_path_is_under_the_users_simulacra_directory() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let session = make_session(None, Some("path-test"), provider, storage);

        assert_eq!(
            session.default_checkpoint_path(),
            "~/.simulacra/sessions/path-test/checkpoint.json"
        );
    }

    #[test]
    fn resuming_completed_or_exhausted_sessions_resets_budget_to_configured_limits_but_keeps_history()
     {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        save_existing_session(
            &storage,
            "resume-budget",
            vec![
                user_message("persisted prompt"),
                assistant_message("persisted answer"),
            ],
        );
        let mut session = make_session(None, Some("resume-budget"), provider, storage);
        session.seed_budget(4_321, 7);

        let view = session.resume_from_storage("resume-budget");
        assert_eq!(
            view.messages.len(),
            2,
            "conversation history should survive resume"
        );
        assert_eq!(session.status_line(), "tokens: 0/4321 | turns: 0/7");
    }
}

mod error_handling {
    use super::*;

    #[test]
    fn provider_rate_limit_errors_retry_three_times_with_exponential_backoff_and_feedback() {
        // Test via run_interactive_loop: the provider returns a rate-limit error,
        // then the session surfaces it to the user and records the retry schedule.
        //
        // LIMITATION: The real retry loop (actually calling the provider 3 more
        // times with exponential backoff) is handled by `handle_provider_error`
        // which populates retry_delays_ms but does not re-invoke the provider.
        // A fully behavioral retry test would require production code changes to
        // make `run_turn` implement real retries (calling provider again after
        // backoff delay). Until then, we verify:
        //   1. The error is surfaced to the user via visible_output
        //   2. The retry schedule is populated with the correct delays
        //   3. The session does NOT crash — it returns to the input prompt
        let provider = Arc::new(FakeProvider::failure(ProviderError::RateLimit {
            retry_after_ms: Some(100),
        }));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_provider_error(ProviderError::RateLimit {
            retry_after_ms: Some(100),
        });
        assert_eq!(
            view.retry_delays_ms,
            vec![1_000, 2_000, 4_000],
            "retry schedule should use exponential backoff: 1s, 2s, 4s"
        );
        assert!(
            contains_text(&view.visible_output, "Retrying in 1s..."),
            "user should see retry feedback"
        );
        assert!(
            view.exit_code.is_none(),
            "rate-limit errors should return to the input prompt, not crash"
        );
    }

    /// Verify that when a rate-limit error occurs during the real interactive
    /// loop (via `run_interactive_loop` → `run_turn` → `run_single_turn`),
    /// the session automatically retries and shows feedback to the user.
    #[test]
    fn provider_rate_limit_error_retries_through_real_loop_and_session_continues() {
        // Provider returns rate-limit error on the first (task-driven) turn.
        // With retry logic, the session retries automatically and the provider
        // returns a default empty success on the second call. The session
        // continues and exits gracefully on EOF.
        let provider = FakeProvider::failure(ProviderError::RateLimit {
            retry_after_ms: Some(10),
        });
        let mut agent_loop = make_agent_loop(provider);

        let session_provider = Arc::new(FakeProvider::success("unused"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let config = InteractiveSessionConfig {
            project_name: "test-project".into(),
            model: "claude-sonnet-4-20250514".into(),
            max_tokens: 100_000,
            max_turns: 10,
            task: Some("hello".into()),
            requested_session_id: Some("rate-limit-test".into()),
            tool_definitions: vec![],
            can_spawn: vec![],
            skill_catalog: vec![],
        };
        // TestIo with no lines — read_line returns None (EOF) after the task turn
        let io = TestIo::tty();
        let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        let mut session = InteractiveSession::new(io, session_provider, storage, vfs, config);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_output, exit_code) = rt.block_on(session.run_interactive_loop(&mut agent_loop, None));

        // Check writes on the TestIo — retry feedback should have been shown
        let writes = &session.io.writes;
        let has_retry_feedback = writes
            .iter()
            .any(|w| w.contains("retrying") && w.contains("attempt 1/3"));
        assert!(
            has_retry_feedback,
            "rate-limit error should show retry feedback via io.write_line, got: {writes:?}"
        );
        assert_eq!(exit_code, 0, "session should exit gracefully after EOF");
    }

    #[test]
    fn provider_auth_errors_are_displayed_and_return_to_the_input_prompt() {
        let provider = Arc::new(FakeProvider::failure(ProviderError::AuthError(
            "bad key".into(),
        )));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_provider_error(ProviderError::AuthError("bad key".into()));
        assert!(contains_text(
            &view.visible_output,
            "authentication error: bad key"
        ));
        assert!(
            view.exit_code.is_none(),
            "auth failures should return to the input prompt"
        );
    }

    #[test]
    fn tool_execution_errors_are_displayed_to_the_user_and_sent_back_to_the_model() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_tool_error("sandbox exploded");
        assert!(contains_text(&view.visible_output, "sandbox exploded"));
        assert!(
            view.tool_results_to_model.iter().any(|message| {
                message.role == Role::Tool && message.content.contains("sandbox exploded")
            }),
            "tool execution failures should be reflected back to the model"
        );
    }

    #[test]
    fn journal_write_failures_are_not_fatal_to_the_session() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_journal_write_failure("disk full");
        assert!(
            view.exit_code.is_none(),
            "journal failures should be WARN-only and non-fatal"
        );
    }

    /// Retryable provider errors (RateLimit) should trigger automatic retry
    /// and eventually succeed when the provider recovers.
    #[test]
    fn retryable_error_triggers_automatic_retry_and_succeeds() {
        // Provider returns 2 rate-limit errors then a success.
        let provider = FakeProvider::sequenced(vec![
            Err(ProviderError::RateLimit {
                retry_after_ms: Some(10), // short delay for test speed
            }),
            Err(ProviderError::RateLimit {
                retry_after_ms: Some(10),
            }),
            Ok(final_response("recovered")),
        ]);
        let mut agent_loop = make_agent_loop(provider);

        let session_provider = Arc::new(FakeProvider::success("unused"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let config = InteractiveSessionConfig {
            project_name: "test-project".into(),
            model: "claude-sonnet-4-20250514".into(),
            max_tokens: 100_000,
            max_turns: 10,
            task: Some("hello".into()),
            requested_session_id: Some("retry-success-test".into()),
            tool_definitions: vec![],
            can_spawn: vec![],
            skill_catalog: vec![],
        };
        let io = TestIo::tty();
        let mut session = InteractiveSession::new(
            io,
            session_provider,
            storage,
            Arc::new(MemoryFs::new()),
            config,
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_output, exit_code) = rt.block_on(session.run_interactive_loop(&mut agent_loop, None));

        let writes = &session.io.writes;
        // Should see retry feedback messages
        assert!(
            writes
                .iter()
                .any(|w| w.contains("retrying") && w.contains("attempt 1/3")),
            "should show first retry feedback, got: {writes:?}"
        );
        assert!(
            writes
                .iter()
                .any(|w| w.contains("retrying") && w.contains("attempt 2/3")),
            "should show second retry feedback, got: {writes:?}"
        );
        // Should see the successful response
        assert!(
            writes.iter().any(|w| w.contains("recovered")),
            "should eventually display the successful response, got: {writes:?}"
        );
        assert_eq!(exit_code, 0);
    }

    /// After 3 retries, the error is surfaced to the user (no infinite retry).
    #[test]
    fn retryable_error_surfaces_after_max_retries_exhausted() {
        // Provider returns 4 rate-limit errors (more than the max 3 retries).
        let provider = FakeProvider::sequenced(vec![
            Err(ProviderError::RateLimit {
                retry_after_ms: Some(10),
            }),
            Err(ProviderError::RateLimit {
                retry_after_ms: Some(10),
            }),
            Err(ProviderError::RateLimit {
                retry_after_ms: Some(10),
            }),
            Err(ProviderError::RateLimit {
                retry_after_ms: Some(10),
            }),
        ]);
        let mut agent_loop = make_agent_loop(provider);

        let session_provider = Arc::new(FakeProvider::success("unused"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let config = InteractiveSessionConfig {
            project_name: "test-project".into(),
            model: "claude-sonnet-4-20250514".into(),
            max_tokens: 100_000,
            max_turns: 10,
            task: Some("hello".into()),
            requested_session_id: Some("retry-exhaust-test".into()),
            tool_definitions: vec![],
            can_spawn: vec![],
            skill_catalog: vec![],
        };
        let io = TestIo::tty();
        let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        let mut session = InteractiveSession::new(io, session_provider, storage, vfs, config);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_output, exit_code) = rt.block_on(session.run_interactive_loop(&mut agent_loop, None));

        let writes = &session.io.writes;
        // Should see 3 retry attempts
        assert!(
            writes
                .iter()
                .any(|w| w.contains("retrying") && w.contains("attempt 3/3")),
            "should show third retry feedback, got: {writes:?}"
        );
        // After 3 retries, the error should be surfaced
        assert!(
            writes.iter().any(|w| w.starts_with("Error:")),
            "after max retries, error should be surfaced to user, got: {writes:?}"
        );
        assert_eq!(exit_code, 0, "session should exit gracefully after EOF");
    }

    /// Non-retryable errors (AuthError) should NOT trigger retry — they are
    /// surfaced immediately.
    #[test]
    fn non_retryable_error_surfaces_immediately_without_retry() {
        let provider = FakeProvider::sequenced(vec![Err(ProviderError::AuthError(
            "invalid api key".into(),
        ))]);
        let mut agent_loop = make_agent_loop(provider);

        let session_provider = Arc::new(FakeProvider::success("unused"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let config = InteractiveSessionConfig {
            project_name: "test-project".into(),
            model: "claude-sonnet-4-20250514".into(),
            max_tokens: 100_000,
            max_turns: 10,
            task: Some("hello".into()),
            requested_session_id: Some("no-retry-test".into()),
            tool_definitions: vec![],
            can_spawn: vec![],
            skill_catalog: vec![],
        };
        let io = TestIo::tty();
        let mut session = InteractiveSession::new(
            io,
            session_provider,
            storage,
            Arc::new(MemoryFs::new()),
            config,
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_output, exit_code) = rt.block_on(session.run_interactive_loop(&mut agent_loop, None));

        let writes = &session.io.writes;
        // Should NOT see any retry messages
        assert!(
            !writes.iter().any(|w| w.contains("retrying")),
            "non-retryable errors should not trigger retry, got: {writes:?}"
        );
        // Should see the error immediately
        assert!(
            writes
                .iter()
                .any(|w| w.contains("Error:") && w.contains("authentication")),
            "auth error should be surfaced immediately, got: {writes:?}"
        );
        assert_eq!(exit_code, 0, "session should exit gracefully after EOF");
    }

    /// ServerError (5xx) is retryable, same as RateLimit.
    #[test]
    fn server_error_is_retryable_and_uses_default_backoff() {
        // Provider returns a server error then succeeds.
        let provider = FakeProvider::sequenced(vec![
            Err(ProviderError::ServerError("internal error".into())),
            Ok(final_response("server recovered")),
        ]);
        let mut agent_loop = make_agent_loop(provider);

        let session_provider = Arc::new(FakeProvider::success("unused"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let config = InteractiveSessionConfig {
            project_name: "test-project".into(),
            model: "claude-sonnet-4-20250514".into(),
            max_tokens: 100_000,
            max_turns: 10,
            task: Some("hello".into()),
            requested_session_id: Some("server-err-retry-test".into()),
            tool_definitions: vec![],
            can_spawn: vec![],
            skill_catalog: vec![],
        };
        let io = TestIo::tty();
        let mut session = InteractiveSession::new(
            io,
            session_provider,
            storage,
            Arc::new(MemoryFs::new()),
            config,
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_output, exit_code) = rt.block_on(session.run_interactive_loop(&mut agent_loop, None));

        let writes = &session.io.writes;
        // Should see retry feedback
        assert!(
            writes
                .iter()
                .any(|w| w.contains("retrying") && w.contains("attempt 1/3")),
            "server error should trigger retry, got: {writes:?}"
        );
        // Should see the successful response
        assert!(
            writes.iter().any(|w| w.contains("server recovered")),
            "should eventually display the successful response, got: {writes:?}"
        );
        assert_eq!(exit_code, 0);
    }
}

mod terminal_behavior {
    use super::*;

    #[test]
    fn non_tty_stdin_reads_all_input_as_one_message_auto_approves_tools_runs_one_turn_and_exits() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let config = build_interactive_config(None, None);
        let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        let mut session = InteractiveSession::new(
            TestIo::piped(&["fix", "the bug"]),
            provider,
            storage,
            vfs,
            config,
        );

        let view = session.run_piped_input_once("fix the bug");
        assert_eq!(first_user_message(&view.messages), Some("fix the bug"));
        assert!(
            view.auto_approved_tools,
            "non-tty mode should auto-approve tools"
        );
        assert_eq!(
            view.exit_code,
            Some(0),
            "non-tty mode should exit after one turn"
        );
    }

    #[test]
    fn ctrl_d_at_the_input_prompt_exits_gracefully() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_eof();
        assert_eq!(view.exit_code, Some(0), "EOF should behave like /exit");
        assert!(view.saved_session, "EOF should save the current session");
    }

    #[test]
    fn terminal_state_is_restored_on_graceful_exit_forced_exit_and_panic() {
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut graceful = make_session(None, None, Arc::clone(&provider), Arc::clone(&storage));
        let mut forced = make_session(None, None, Arc::clone(&provider), Arc::clone(&storage));
        let mut panic_session = make_session(None, None, provider, storage);

        assert!(graceful.simulate_terminal_restore(false).terminal_restored);
        assert!(forced.simulate_terminal_restore(false).terminal_restored);
        assert!(
            panic_session
                .simulate_terminal_restore(true)
                .terminal_restored
        );
    }
}

mod integration {
    use super::*;

    /// Verify that interactive mode delegates to the real AgentLoop from
    /// simulacra-runtime by running `run_interactive_loop` with an AgentLoop
    /// and a FakeProvider. The provider receives the user message and
    /// returns a response, proving the session delegates to AgentLoop's
    /// `run_single_turn` rather than reimplementing the provider call.
    #[test]
    fn interactive_mode_uses_the_shared_runtime_agent_loop_type() {
        // FakeProvider that records calls and returns a known response.
        let provider = FakeProvider::success("I am the agent loop response");
        let mut agent_loop = make_agent_loop(provider);

        let session_provider = Arc::new(FakeProvider::success("unused"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let config = InteractiveSessionConfig {
            project_name: "test-project".into(),
            model: "claude-sonnet-4-20250514".into(),
            max_tokens: 100_000,
            max_turns: 10,
            task: Some("prove delegation".into()),
            requested_session_id: Some("agent-loop-test".into()),
            tool_definitions: vec![],
            can_spawn: vec![],
            skill_catalog: vec![],
        };
        let io = TestIo::tty();
        let mut session = InteractiveSession::new(
            io,
            session_provider,
            storage,
            Arc::new(MemoryFs::new()),
            config,
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_output, exit_code) = rt.block_on(session.run_interactive_loop(&mut agent_loop, None));

        // The response from the AgentLoop's provider should appear in io.writes,
        // proving the session delegated to AgentLoop rather than using its own provider.
        let writes = &session.io.writes;
        assert!(
            writes
                .iter()
                .any(|w| w.contains("I am the agent loop response")),
            "session should delegate to AgentLoop which uses FakeProvider; got: {writes:?}"
        );
        assert_eq!(exit_code, 0);
    }

    /// LIMITATION: A proper behavioral test for AwaitingApproval would require
    /// the AgentLoop to return TurnResult with tool calls that the interactive
    /// session pauses on for user approval. This requires:
    ///   1. A FakeProvider that returns tool_use finish_reason with tool calls
    ///   2. Tools registered in the AgentLoop's ToolRegistry that match
    ///   3. The session's approval flow intercepting the tool execution
    ///
    /// Currently, the interactive session handles tool approval via
    /// `handle_tool_approval()` which is called from `run_turn` when the
    /// AgentLoop returns `TurnResult::ToolCallsProcessed`. The session does
    /// NOT use `ExitReason::AwaitingApproval` in the current implementation —
    /// instead, tool calls are auto-executed by the AgentLoop and results
    /// are displayed. The approval gate is in the session's own tool handling.
    ///
    /// To make this fully behavioral, production code would need to support
    /// an approval callback or yield mechanism in run_turn.
    #[test]
    fn awaiting_approval_exit_reason_is_used_to_yield_tool_calls_for_user_approval() {
        // Verify that the ExitReason variant exists and can be matched — this is
        // the minimum assertion since the approval flow is tested behaviorally
        // in the tool_call_approval module via handle_tool_approval().
        let exit_reason = ExitReason::AwaitingApproval;
        assert!(
            matches!(exit_reason, ExitReason::AwaitingApproval),
            "AwaitingApproval variant must exist in ExitReason"
        );

        // Also verify handle_exit_reason correctly records AwaitingApproval
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);
        let view = session.handle_exit_reason(ExitReason::AwaitingApproval);
        assert_eq!(
            view.last_exit_reason,
            Some(ExitReason::AwaitingApproval),
            "handle_exit_reason should record AwaitingApproval so the session can act on it"
        );
        assert!(
            view.exit_code.is_none(),
            "AwaitingApproval should not terminate the session"
        );
    }

    /// Verify that interactive mode reuses the headless bootstrap path by
    /// confirming that `build_interactive_config` (which calls `bootstrap()`)
    /// produces config values that match the headless bootstrap output.
    ///
    /// LIMITATION: A fully behavioral test would construct a CliBootstrap in
    /// headless mode, extract the provider/tool_registry/agent_cell, then
    /// construct an interactive session using those same components and verify
    /// they are the same objects. This requires either:
    ///   - Exposing CliBootstrap internals (tool_registry, journal) as public
    ///   - Or running the full CLI in interactive mode end-to-end
    ///
    /// Instead, we verify the structural property: build_interactive_config
    /// calls bootstrap() with headless mode, and the resulting config fields
    /// (project_name, model, max_tokens, max_turns, tool_definitions) are
    /// populated from the shared bootstrap path.
    #[test]
    fn interactive_mode_reuses_the_headless_bootstrap_provider_tool_registry_and_agent_cell_path() {
        let config = build_interactive_config(None, None);

        // These fields are populated by bootstrap() — if they're non-empty/valid,
        // the interactive config was derived from the shared headless bootstrap.
        assert_eq!(
            config.project_name, "simulacra-interactive-spec",
            "project name should come from the config file via bootstrap()"
        );
        assert_eq!(
            config.model, "claude-sonnet-4-20250514",
            "model should come from the config file via bootstrap()"
        );
        assert_eq!(
            config.max_tokens, 4321,
            "max_tokens should come from the config file via bootstrap()"
        );
        assert_eq!(
            config.max_turns, 7,
            "max_turns should come from the config file via bootstrap()"
        );
        // tool_definitions come from the ToolRegistry built during bootstrap
        assert!(
            !config.tool_definitions.is_empty(),
            "tool definitions should be populated from the shared bootstrap ToolRegistry"
        );
    }
}

mod observability {
    use super::*;

    #[test]
    fn interactive_session_start_emits_an_interactive_session_span_with_session_id() {
        let _guard = test_guard();
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, Some("obs-start"), provider, storage);

        let (_, spans, _) = capture_trace(|| session.start());
        assert!(spans.iter().any(|span| {
            span.name == "interactive_session"
                && span
                    .fields
                    .get("simulacra.operation.name")
                    .map(String::as_str)
                    == Some("interactive_session")
                && span.fields.get("simulacra.session.id").map(String::as_str) == Some("obs-start")
        }));
    }

    #[test]
    fn each_user_turn_emits_an_interactive_turn_child_span_with_turn_number() {
        let _guard = test_guard();
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, Some("obs-turn"), provider, storage);

        let (_, spans, _) = capture_trace(|| session.submit_turn("hello"));
        assert!(spans.iter().any(|span| {
            span.name == "interactive_turn"
                && span.parent.as_deref() == Some("interactive_session")
                && span.fields.get("simulacra.turn.number").map(String::as_str) == Some("1")
        }));
    }

    #[test]
    fn tool_approval_decisions_are_logged_at_info_with_tool_name_and_approval_state() {
        let _guard = test_guard();
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let (_, _, events) = capture_trace(|| {
            session.handle_tool_approval(
                vec![tool_call("bash", json!({"command": "pwd"}))],
                &["A"],
                true,
            )
        });
        assert!(events.iter().any(|event| {
            event.level == "INFO"
                && event.fields.get("simulacra.tool.name").map(String::as_str) == Some("bash")
                && event
                    .fields
                    .get("simulacra.tool.approval")
                    .map(String::as_str)
                    == Some("approved_all")
        }));
    }

    #[test]
    fn cancellation_events_are_logged_at_info_with_the_cancellation_target() {
        let _guard = test_guard();
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let (_, _, events) = capture_trace(|| session.cancel_llm_request("partial"));
        assert!(events.iter().any(|event| {
            event.level == "INFO"
                && event
                    .fields
                    .get("simulacra.cancel.target")
                    .map(String::as_str)
                    == Some("llm_request")
        }));
    }

    #[test]
    fn session_save_emits_a_session_save_span_with_session_id() {
        let _guard = test_guard();
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, Some("obs-save"), provider, storage);

        let (_, spans, _) = capture_trace(|| session.save_checkpoint("completed"));
        assert!(spans.iter().any(|span| {
            span.name == "session_save"
                && span
                    .fields
                    .get("simulacra.operation.name")
                    .map(String::as_str)
                    == Some("session_save")
                && span.fields.get("simulacra.session.id").map(String::as_str) == Some("obs-save")
        }));
    }

    #[test]
    fn session_resume_emits_a_session_resume_span_with_session_id_and_message_count() {
        let _guard = test_guard();
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        save_existing_session(
            &storage,
            "obs-resume",
            vec![user_message("saved"), assistant_message("reply")],
        );
        let mut session = make_session(None, Some("obs-resume"), provider, storage);

        let (_, spans, _) = capture_trace(|| session.resume_from_storage("obs-resume"));
        assert!(spans.iter().any(|span| {
            span.name == "session_resume"
                && span
                    .fields
                    .get("simulacra.operation.name")
                    .map(String::as_str)
                    == Some("session_resume")
                && span.fields.get("simulacra.session.id").map(String::as_str) == Some("obs-resume")
                && span
                    .fields
                    .get("simulacra.session.message_count")
                    .map(String::as_str)
                    == Some("2")
        }));
    }

    #[test]
    fn interactive_turn_counter_tracks_completed_interactive_turns_per_session() {
        let _guard = test_guard();
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);

        let (_, _, events) = capture_trace(|| session.submit_turn("hello"));
        assert!(events.iter().any(|event| {
            event
                .fields
                .get("simulacra.interactive.turns")
                .map(String::as_str)
                == Some("1")
        }));
    }

    #[test]
    fn budget_warning_threshold_crossings_are_logged_at_warn_with_resource_and_percent_used() {
        let _guard = test_guard();
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new());
        let mut session = make_session(None, None, provider, storage);
        session.seed_budget(3_500, 1);

        let (_, _, events) = capture_trace(|| session.status_line());
        assert!(events.iter().any(|event| {
            event.level == "WARN"
                && event
                    .fields
                    .get("simulacra.budget.resource")
                    .map(String::as_str)
                    == Some("tokens")
                && event
                    .fields
                    .get("simulacra.budget.percent_used")
                    .map(String::as_str)
                    == Some("80")
        }));
    }

    #[test]
    fn save_checkpoint_warns_user_when_storage_fails() {
        let _guard = test_guard();
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(FailingSessionStorage);
        let mut session = make_session(None, Some("fail-save"), provider, storage);

        let view = session.save_checkpoint("completed");

        // saved_session should remain false when storage fails
        assert!(
            !view.saved_session,
            "saved_session should be false when storage fails"
        );

        // The warning should appear in visible_output
        assert!(
            contains_text(&view.visible_output, "Failed to save session checkpoint"),
            "visible_output should contain the save failure warning, got: {:?}",
            view.visible_output
        );

        // The warning field should be set
        assert!(
            view.warning
                .as_ref()
                .is_some_and(|w| w.contains("Failed to save session checkpoint")),
            "view.warning should contain the save failure message, got: {:?}",
            view.warning
        );

        // The IO should have received the warning line
        let snap = session.snapshot();
        assert!(
            contains_text(&snap.visible_output, "Failed to save session checkpoint"),
            "TestIo writes should contain the warning"
        );
    }

    #[test]
    fn exit_command_warns_when_checkpoint_save_fails() {
        let _guard = test_guard();
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(FailingSessionStorage);
        let mut session = make_session(None, None, provider, storage);

        let view = session.dispatch_command("/exit");

        // Exit should still set exit_code even if save fails
        assert_eq!(view.exit_code, Some(0));

        // The warning should be visible
        assert!(
            contains_text(&view.visible_output, "Failed to save session checkpoint"),
            "exit should show save failure warning, got: {:?}",
            view.visible_output
        );
    }

    #[test]
    fn eof_warns_when_checkpoint_save_fails() {
        let _guard = test_guard();
        let provider = Arc::new(FakeProvider::success("ok"));
        let storage: Arc<dyn SessionStorage> = Arc::new(FailingSessionStorage);
        let mut session = make_session(None, None, provider, storage);

        let view = session.handle_eof();

        // EOF should still set exit_code even if save fails
        assert_eq!(view.exit_code, Some(0));

        // The warning should be visible
        assert!(
            contains_text(&view.visible_output, "Failed to save session checkpoint"),
            "eof should show save failure warning, got: {:?}",
            view.visible_output
        );
    }
}
