use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use clap::Parser;
use serde_json::{Value, json};
use simulacra_cli::{CliArgs, CliMode, OutputFormat, run, run_with_provider};
use simulacra_types::{
    ActivityEvent, FinishReason, Message, Provider, ProviderError, ProviderResponse,
    ProviderStreamEvent, ProviderStreamSink, ResourceBudget, Role, StreamingProvider, TokenUsage,
    ToolCallMessage, ToolDefinition,
};
use tempfile::TempDir;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

struct SpanCaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
static CAPTURED_SPANS: OnceLock<Arc<Mutex<Vec<CapturedSpan>>>> = OnceLock::new();
static CAPTURE_INSTALL: OnceLock<()> = OnceLock::new();

impl<S> tracing_subscriber::Layer<S> for SpanCaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
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
        let Some(span_ref) = ctx.span(id) else {
            return;
        };
        let span_name = span_ref.name().to_string();
        let mut new_fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut new_fields);
        values.record(&mut visitor);

        let mut spans = self.spans.lock().unwrap();
        if let Some(span) = spans.iter_mut().rev().find(|span| span.name == span_name) {
            span.fields.extend(new_fields);
        }
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

fn capture_store() -> Arc<Mutex<Vec<CapturedSpan>>> {
    CAPTURE_INSTALL.get_or_init(|| {
        let spans = Arc::new(Mutex::new(Vec::new()));
        CAPTURED_SPANS
            .set(Arc::clone(&spans))
            .expect("capture store should only be initialized once");

        let subscriber =
            tracing_subscriber::registry::Registry::default().with(SpanCaptureLayer { spans });
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber should install");
    });

    Arc::clone(
        CAPTURED_SPANS
            .get()
            .expect("capture store should be installed"),
    )
}

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = capture_store();
    guard
}

#[derive(Debug)]
struct TempConfig {
    _dir: TempDir,
    path: PathBuf,
}

impl TempConfig {
    fn write(contents: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp config dir should be created");
        let path = dir.path().join("simulacra.toml");
        fs::write(&path, contents).expect("temp config should be written");
        Self { _dir: dir, path }
    }

    fn missing() -> Self {
        let dir = tempfile::tempdir().expect("temp config dir should be created");
        let path = dir.path().join("missing.toml");
        Self { _dir: dir, path }
    }

    fn path_string(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

#[derive(Debug)]
struct FakeProvider {
    responses: Mutex<Vec<Result<ProviderResponse, ProviderError>>>,
    stream_events: Mutex<Vec<Vec<ProviderStreamEvent>>>,
}

impl FakeProvider {
    fn success(text: &str) -> Self {
        Self::scripted(vec![Ok(final_response(text, 11, 7))], vec![Vec::new()])
    }

    fn failure(message: &str) -> Self {
        Self::scripted(
            vec![Err(ProviderError::Other(message.to_string()))],
            vec![Vec::new()],
        )
    }

    fn scripted(
        responses: Vec<Result<ProviderResponse, ProviderError>>,
        stream_events: Vec<Vec<ProviderStreamEvent>>,
    ) -> Self {
        Self {
            responses: Mutex::new(responses),
            stream_events: Mutex::new(stream_events),
        }
    }

    fn next_response(&self) -> Result<ProviderResponse, ProviderError> {
        let mut responses = self
            .responses
            .lock()
            .map_err(|error| ProviderError::Other(format!("poisoned fake provider: {error}")))?;
        if responses.is_empty() {
            return Err(ProviderError::Other(
                "fake provider response script exhausted".into(),
            ));
        }
        responses.remove(0)
    }

    fn next_stream_events(&self) -> Result<Vec<ProviderStreamEvent>, ProviderError> {
        let mut events = self.stream_events.lock().map_err(|error| {
            ProviderError::Other(format!("poisoned fake stream script: {error}"))
        })?;
        if events.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(events.remove(0))
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
        Box::pin(async move { self.next_response() })
    }

    fn as_streaming(&self) -> Option<&dyn StreamingProvider> {
        Some(self)
    }
}

impl StreamingProvider for FakeProvider {
    fn chat_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
        sink: &'a dyn ProviderStreamSink,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            for event in self.next_stream_events()? {
                sink.emit(event);
                tokio::task::yield_now().await;
            }
            self.next_response()
        })
    }
}

fn final_response(text: &str, input_tokens: u64, output_tokens: u64) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: text.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens,
            output_tokens,
        },
        finish_reason: FinishReason::EndTurn,
        provider_response_id: Some("resp-final".into()),
        model: "claude-sonnet-4-20250514".into(),
    }
}

fn tool_call_response() -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCallMessage {
                id: "call-echo".into(),
                name: "shell_exec".into(),
                arguments: json!({"command": "printf 'echo-line\\n'"}),
            }],
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
        },
        finish_reason: FinishReason::ToolUse,
        provider_response_id: Some("resp-tool".into()),
        model: "claude-sonnet-4-20250514".into(),
    }
}

fn spawn_agent_tool_call_response() -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCallMessage {
                id: "call-spawn-researcher".into(),
                name: "spawn_agent".into(),
                arguments: json!({
                    "agent_type": "researcher",
                    "task": "summarize the fixture",
                    "budget": {
                        "max_tokens": 128,
                        "max_turns": 1,
                        "max_cost": "0",
                        "max_sub_agents": 0
                    }
                }),
            }],
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
        },
        finish_reason: FinishReason::ToolUse,
        provider_response_id: Some("resp-spawn".into()),
        model: "claude-sonnet-4-20250514".into(),
    }
}

fn valid_config_toml(task: Option<&str>) -> String {
    let task_line = task
        .map(|task| format!("task = {task:?}"))
        .unwrap_or_default();

    format!(
        r#"[project]
name = "simulacra-cli-jsonl-spec"

[agent_types.default]
model = "claude-sonnet-4-20250514"
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

fn spawn_capable_config_toml() -> String {
    r#"[project]
name = "simulacra-cli-jsonl-spawn-spec"

[agent_types.default]
model = "claude-sonnet-4-20250514"
max_turns = 7
max_tokens = 4321
max_sub_agents = 2
can_spawn = ["researcher"]

[agent_types.default.capabilities]
shell = true
javascript = true
paths_read = ["/workspace/**"]
paths_write = ["/workspace/**"]

[agent_types.researcher]
model = "claude-sonnet-4-20250514"
max_turns = 1
max_tokens = 128

[agent_types.researcher.capabilities]
paths_read = ["/workspace/**"]

[task]
entry_agent = "default"
"#
    .to_string()
}

fn args_with_output_format(
    config_path: String,
    task: Option<&str>,
    format: OutputFormat,
) -> CliArgs {
    CliArgs {
        config_path,
        task: task.map(str::to_string),
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: true,
        output_format: format,
    }
}

fn jsonl_args(config_path: String, task: &str) -> CliArgs {
    args_with_output_format(config_path, Some(task), OutputFormat::Jsonl)
}

fn parse_jsonl(stdout: &str) -> Vec<Value> {
    assert!(!stdout.is_empty(), "JSONL stdout should not be empty");
    assert!(
        stdout.ends_with('\n'),
        "every JSONL line, including the result, should be newline terminated: {stdout:?}"
    );

    stdout
        .split_inclusive('\n')
        .enumerate()
        .map(|(index, line)| {
            assert!(
                line.ends_with('\n'),
                "line {index} should be newline terminated: {line:?}"
            );
            let line = line.trim_end_matches('\n');
            assert!(
                !line.contains('\n'),
                "line {index} should be one compact JSON object without embedded newlines"
            );
            serde_json::from_str::<Value>(line).unwrap_or_else(|error| {
                panic!("line {index} should be valid JSON: {error}: {line}")
            })
        })
        .collect()
}

fn activity_types(lines: &[Value]) -> Vec<&str> {
    lines
        .iter()
        .filter(|line| line["kind"] == "activity")
        .map(|line| {
            line["event"]["type"]
                .as_str()
                .expect("activity event should have a serde-tagged type")
        })
        .collect()
}

fn last_result(lines: &[Value]) -> &Value {
    let result = lines
        .last()
        .expect("JSONL output should contain a result line");
    assert_eq!(result["kind"], "result");
    assert!(
        lines[..lines.len().saturating_sub(1)]
            .iter()
            .all(|line| line["kind"] == "activity"),
        "only the final line should be the terminal result envelope: {lines:#?}"
    );
    result
}

fn index_of_type(types: &[&str], needle: &str) -> usize {
    types
        .iter()
        .position(|event_type| *event_type == needle)
        .unwrap_or_else(|| panic!("missing activity event type {needle:?} in {types:?}"))
}

fn field_matches(span: &CapturedSpan, key: &str, expected: &str) -> bool {
    span.fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

fn run_with_provider_bounded(
    args: CliArgs,
    provider: Box<dyn Provider>,
    timeout: Duration,
) -> simulacra_cli::CliOutput {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_with_provider(args, provider));
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("CLI run should return CliOutput, got error: {error}"),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("headless spawn_agent run hung for more than {timeout:?}")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("headless spawn_agent runner exited without sending a result")
        }
    }
}

/// S055 argument parsing assertions: `--output-format` accepts `text` and
/// `jsonl`, unknown values are clap errors, and the default is `text`.
#[test]
fn output_format_argument_parses_values_defaults_and_rejects_unknowns() {
    let _guard = test_guard();

    let text = CliArgs::try_parse_from(["simulacra", "--task", "hello", "--output-format", "text"])
        .expect("text output format should parse");
    assert_eq!(text.output_format, OutputFormat::Text);

    let jsonl =
        CliArgs::try_parse_from(["simulacra", "--task", "hello", "--output-format", "jsonl"])
            .expect("jsonl output format should parse");
    assert_eq!(jsonl.output_format, OutputFormat::Jsonl);

    let default =
        CliArgs::try_parse_from(["simulacra", "--task", "hello"]).expect("task should parse");
    assert_eq!(default.output_format, OutputFormat::Text);

    let err = CliArgs::try_parse_from(["simulacra", "--task", "hello", "--output-format", "yaml"])
        .expect_err("unknown output format should be rejected by clap");
    assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
}

/// S055 argument parsing assertion: `--mode interactive --output-format jsonl`
/// is accepted and the JSONL flag is ignored by the interactive terminal path.
#[test]
fn jsonl_output_format_is_accepted_but_ignored_in_interactive_mode() {
    let _guard = test_guard();

    let parsed = CliArgs::try_parse_from([
        "simulacra",
        "--mode",
        "interactive",
        "--output-format",
        "jsonl",
        "--task",
        "hello",
    ])
    .expect("interactive jsonl invocation should parse");
    assert_eq!(parsed.mode, Some(CliMode::Interactive));
    assert_eq!(parsed.output_format, OutputFormat::Jsonl);

    let config = TempConfig::missing();
    let output = run_with_provider(
        CliArgs {
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
            no_catalog: true,
            output_format: OutputFormat::Jsonl,
        },
        Box::new(FakeProvider::success("not used by terminal startup")),
    )
    .expect("interactive mode should return CliOutput rather than rejecting jsonl");

    assert!(
        !output.stderr_content.contains("output-format"),
        "interactive startup may fail for terminal reasons, but not because jsonl was requested: {:?}",
        output.stderr_content
    );
}

/// S055 argument parsing and non-goal assertions: text output remains the
/// default and current text-mode stdout behavior is unchanged.
#[test]
fn default_text_output_keeps_existing_final_message_stdout_contract() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml(None));

    let output = run_with_provider(
        args_with_output_format(config.path_string(), Some("say hello"), OutputFormat::Text),
        Box::new(FakeProvider::success("plain answer")),
    )
    .expect("text run should return cli output");

    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout_content, "plain answer");
}

/// S055 headless streaming assertions: JSONL mode uses a channel activity sink,
/// streams activity in agent-loop order, writes compact newline-terminated JSON
/// envelopes, drains activity before the terminal result, and treats
/// `TurnComplete` as a normal activity event rather than the terminator.
#[test]
fn jsonl_headless_streams_tokens_tool_events_turn_complete_then_result_in_order() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml(None));
    let provider = FakeProvider::scripted(
        vec![
            Ok(tool_call_response()),
            Ok(final_response("echo complete", 13, 17)),
        ],
        vec![
            vec![
                ProviderStreamEvent::TextDelta { text: "Hel".into() },
                ProviderStreamEvent::TextDelta { text: "lo".into() },
            ],
            Vec::new(),
        ],
    );

    let output = run_with_provider(
        jsonl_args(config.path_string(), "run the echo tool"),
        Box::new(provider),
    )
    .expect("jsonl run should return cli output");

    assert_eq!(output.exit_code, 0);
    let lines = parse_jsonl(&output.stdout_content);
    let result = last_result(&lines);
    assert_eq!(result["ok"], true);

    let types = activity_types(&lines);
    assert_eq!(types[0], "Token");
    assert_eq!(types[1], "Token");
    assert!(
        index_of_type(&types, "Token") < index_of_type(&types, "ToolStart"),
        "provider token deltas should arrive before the tool starts: {types:?}"
    );
    assert!(
        index_of_type(&types, "ToolStart") < index_of_type(&types, "ToolOutput"),
        "ToolStart should precede ToolOutput: {types:?}"
    );
    assert!(
        index_of_type(&types, "ToolOutput") < index_of_type(&types, "ToolFinish"),
        "ToolOutput should precede ToolFinish: {types:?}"
    );
    assert!(
        index_of_type(&types, "ToolFinish") < index_of_type(&types, "TurnComplete"),
        "ToolFinish should precede TurnComplete: {types:?}"
    );
    assert_eq!(
        types.last().copied(),
        Some("TurnComplete"),
        "TurnComplete should be the final activity line immediately before result"
    );

    let tool_output = lines
        .iter()
        .find(|line| line["event"]["type"] == "ToolOutput")
        .expect("tool output activity should be emitted");
    assert!(
        tool_output["event"]["line"]
            .as_str()
            .expect("tool output line should be a string")
            .contains("echo-line"),
        "echo tool output should be preserved in the activity event: {tool_output:#?}"
    );
}

#[test]
fn jsonl_headless_spawn_agent_returns_handle_and_emits_child_spawned_without_hanging() {
    let _guard = test_guard();
    let config = TempConfig::write(&spawn_capable_config_toml());
    let provider = FakeProvider::scripted(
        vec![
            Ok(spawn_agent_tool_call_response()),
            Ok(final_response("spawn accepted", 13, 17)),
        ],
        vec![Vec::new(), Vec::new()],
    );

    let output = run_with_provider_bounded(
        jsonl_args(config.path_string(), "delegate to a researcher"),
        Box::new(provider),
        Duration::from_secs(2),
    );

    assert_eq!(output.exit_code, 0);
    let lines = parse_jsonl(&output.stdout_content);
    let result = last_result(&lines);
    assert_eq!(result["ok"], true);
    assert_eq!(result["final_message"], "spawn accepted");

    let child_spawned = lines
        .iter()
        .find(|line| line["event"]["type"] == "ChildSpawned")
        .unwrap_or_else(|| panic!("spawn_agent should emit ChildSpawned activity: {lines:#?}"));
    let child_id = child_spawned["event"]["child_id"]
        .as_str()
        .expect("ChildSpawned should include child_id");
    assert!(child_id.starts_with("child-researcher-"));
    assert_eq!(child_spawned["event"]["agent_type"], "researcher");
    assert_eq!(child_spawned["event"]["task"], "summarize the fixture");

    let tool_output = lines
        .iter()
        .find(|line| line["event"]["type"] == "ToolOutput")
        .unwrap_or_else(|| {
            panic!("spawn_agent handle should be emitted as tool output: {lines:#?}")
        });
    let tool_output_line = tool_output["event"]["line"]
        .as_str()
        .expect("ToolOutput line should be a string");
    let handle: Value =
        serde_json::from_str(tool_output_line).expect("spawn_agent tool output should be JSON");
    assert_eq!(handle["child_id"], child_id);
    assert_eq!(handle["agent_type"], "researcher");
    assert_eq!(handle["status"], "running");

    let types = activity_types(&lines);
    assert!(
        index_of_type(&types, "ChildSpawned") < index_of_type(&types, "ToolOutput"),
        "ChildSpawned should be emitted before the parent-visible handle result: {types:?}"
    );
}

/// S055 envelope schema assertions: every stdout line is valid JSON, activity
/// envelopes have `{"kind":"activity","event":{...}}`, result envelopes have
/// `{"kind":"result",...}`, and the result line is always last.
#[test]
fn jsonl_stdout_lines_are_activity_or_terminal_result_envelopes() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml(None));
    let provider = FakeProvider::scripted(
        vec![Ok(final_response("hello world", 2, 4))],
        vec![vec![
            ProviderStreamEvent::ThinkingStart,
            ProviderStreamEvent::ThinkingDelta {
                text: "considering".into(),
            },
            ProviderStreamEvent::ThinkingEnd,
            ProviderStreamEvent::TextDelta {
                text: "hello world".into(),
            },
        ]],
    );

    let output = run_with_provider(
        jsonl_args(config.path_string(), "think"),
        Box::new(provider),
    )
    .expect("jsonl run should return cli output");

    assert_eq!(output.exit_code, 0);
    let lines = parse_jsonl(&output.stdout_content);
    let result = last_result(&lines);
    assert_eq!(result["kind"], "result");

    for line in &lines[..lines.len() - 1] {
        assert_eq!(line["kind"], "activity");
        assert!(
            line.get("event").is_some_and(Value::is_object),
            "activity envelope should contain an event object: {line:#?}"
        );
        assert!(
            line["event"].get("type").is_some_and(Value::is_string),
            "ActivityEvent serde tag should be preserved: {line:#?}"
        );
    }

    assert_eq!(
        activity_types(&lines),
        vec![
            "ThinkStart",
            "ThinkDelta",
            "ThinkEnd",
            "Token",
            "TurnComplete"
        ]
    );
}

/// S055 result success and exit-code assertions: success emits `ok: true`, the
/// last assistant message as `final_message`, `used_turns`, total tokens, and
/// exit code 0 in both `CliOutput` and the terminal JSONL result.
#[test]
fn jsonl_success_result_reports_final_message_turns_tokens_and_exit_code() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml(None));
    let provider = FakeProvider::scripted(
        vec![Ok(final_response("final answer", 19, 23))],
        vec![vec![ProviderStreamEvent::TextDelta {
            text: "final answer".into(),
        }]],
    );

    let output = run_with_provider(
        jsonl_args(config.path_string(), "answer"),
        Box::new(provider),
    )
    .expect("jsonl run should return cli output");

    assert_eq!(output.exit_code, 0);
    let lines = parse_jsonl(&output.stdout_content);
    let result = last_result(&lines);
    assert_eq!(result["ok"], true);
    assert_eq!(result["final_message"], "final answer");
    assert_eq!(result["turns"], 1);
    assert_eq!(result["tokens"], 42);
    assert_eq!(result["exit_code"], 0);
}

/// S055 result failure and exit-code assertions: provider failure emits a
/// terminal JSONL result with `ok: false`, `final_message: null`, the same
/// display error text text mode reports, consumption counters when available,
/// and exit code 1.
#[test]
fn jsonl_provider_failure_result_reports_error_and_exit_code_without_final_message() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml(None));
    let output = run_with_provider(
        jsonl_args(config.path_string(), "fail"),
        Box::new(FakeProvider::failure("upstream unavailable")),
    )
    .expect("jsonl run should return cli output");

    assert_eq!(output.exit_code, 1);
    let lines = parse_jsonl(&output.stdout_content);
    assert_eq!(
        lines.len(),
        1,
        "provider failure before events should still emit result"
    );
    let result = last_result(&lines);
    assert_eq!(result["ok"], false);
    assert_eq!(result["final_message"], Value::Null);
    assert_eq!(
        result["error"],
        "provider error: other: upstream unavailable"
    );
    assert_eq!(result["turns"], 0);
    assert_eq!(result["tokens"], 0);
    assert_eq!(result["exit_code"], 1);
}

/// S055 stdout/stderr separation assertions: JSONL mode stdout contains only
/// JSON envelopes, verbose tracing remains on stderr, and a line-oriented JSON
/// parser can consume stdout without seeing stray tracing or banners.
#[test]
fn jsonl_verbose_mode_keeps_stdout_parseable_and_tracing_on_stderr() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml(None));
    let mut args = jsonl_args(config.path_string(), "verbose answer");
    args.verbose = true;

    let output = run_with_provider(args, Box::new(FakeProvider::success("verbose final")))
        .expect("jsonl run should return cli output");

    assert_eq!(output.exit_code, 0);
    let lines = parse_jsonl(&output.stdout_content);
    assert_eq!(last_result(&lines)["final_message"], "verbose final");
    assert!(
        output.stderr_content.contains("DEBUG"),
        "verbose mode should still report tracing/debug output on stderr: {:?}",
        output.stderr_content
    );
}

/// S055 bootstrap failure assertions: failures before bootstrap/provider build
/// behave exactly like text mode, with no JSONL stdout and exit code 1.
#[test]
fn jsonl_bootstrap_failures_emit_no_stdout_and_exit_one() {
    let _guard = test_guard();

    let missing = TempConfig::missing();
    let missing_output = run(CliArgs {
        config_path: missing.path_string(),
        task: None,
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: true,
        output_format: OutputFormat::Jsonl,
    })
    .expect("bootstrap failure should return cli output");
    assert_eq!(missing_output.exit_code, 1);
    assert_eq!(missing_output.stdout_content, "");

    let invalid = TempConfig::write("this = [ definitely not valid toml");
    let invalid_output = run(CliArgs {
        config_path: invalid.path_string(),
        task: Some("hello".into()),
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: true,
        output_format: OutputFormat::Jsonl,
    })
    .expect("invalid TOML should return cli output");
    assert_eq!(invalid_output.exit_code, 1);
    assert_eq!(invalid_output.stdout_content, "");
    assert!(
        invalid_output
            .stderr_content
            .contains("failed to parse TOML"),
        "stderr should explain the bootstrap failure: {:?}",
        invalid_output.stderr_content
    );
}

/// S055 approval/input limitation assertions: headless JSONL mirrors text
/// headless mode by not attaching a HITL runtime, so tool calls auto-run with
/// no approval gate and approval/input activity variants remain ordinary
/// serializable ActivityEvent payloads.
#[test]
fn jsonl_headless_does_not_require_approval_and_activity_variants_stay_serializable() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml(None));
    let provider = FakeProvider::scripted(
        vec![
            Ok(tool_call_response()),
            Ok(final_response("tool finished", 7, 8)),
        ],
        vec![Vec::new(), Vec::new()],
    );

    let output = run_with_provider(
        jsonl_args(config.path_string(), "run without asking"),
        Box::new(provider),
    )
    .expect("jsonl run should return cli output");
    assert_eq!(output.exit_code, 0);

    let lines = parse_jsonl(&output.stdout_content);
    let types = activity_types(&lines);
    assert!(types.contains(&"ToolStart"));
    assert!(types.contains(&"ToolFinish"));
    assert!(
        !types.contains(&"ToolApprovalRequired"),
        "headless mode should not attach an approval runtime: {types:?}"
    );

    for event in [
        ActivityEvent::ToolApprovalRequired {
            tool_call_id: "call-needs-approval".into(),
            name: "shell_exec".into(),
            arguments: json!({"command": "echo blocked"}),
            reason: Some("policy".into()),
        },
        ActivityEvent::InputRequired {
            prompt: "provide value".into(),
            schema: Some(json!({"type": "string"})),
        },
    ] {
        let encoded = json!({"kind": "activity", "event": event});
        assert_eq!(encoded["kind"], "activity");
        assert!(
            encoded["event"]["type"].is_string(),
            "approval/input variants should remain normal ActivityEvent payloads: {encoded:#?}"
        );
    }
}

/// S055 child-agent and workflow assertions: child-agent and workflow
/// `ActivityEvent` variants are preserved unchanged by the envelope schema,
/// including recursively nested child activity.
#[test]
fn activity_envelopes_preserve_child_and_workflow_event_payloads() {
    let child = json!({
        "kind": "activity",
        "event": ActivityEvent::ChildActivity {
            child_id: "child-1".into(),
            agent_type: "researcher".into(),
            event: Box::new(ActivityEvent::Token { text: "nested".into() }),
        }
    });
    assert_eq!(child["event"]["type"], "ChildActivity");
    assert_eq!(child["event"]["child_id"], "child-1");
    assert_eq!(child["event"]["event"]["type"], "Token");
    assert_eq!(child["event"]["event"]["text"], "nested");

    let workflow_events = [
        ActivityEvent::WorkflowStarted {
            run_id: "run-1".into(),
            script_path: "/workflow.js".into(),
            name: "demo".into(),
        },
        ActivityEvent::WorkflowProgress {
            run_id: "run-1".into(),
            message: "halfway".into(),
        },
        ActivityEvent::WorkflowPhaseStarted {
            run_id: "run-1".into(),
            name: "phase".into(),
        },
        ActivityEvent::WorkflowPhaseCompleted {
            run_id: "run-1".into(),
            name: "phase".into(),
        },
        ActivityEvent::WorkflowAgentStarted {
            run_id: "run-1".into(),
            key: "worker".into(),
            agent: Some("default".into()),
            task: Some("do work".into()),
        },
        ActivityEvent::WorkflowAgentCompleted {
            run_id: "run-1".into(),
            key: "worker".into(),
            cached: false,
            is_error: false,
        },
        ActivityEvent::WorkflowCompleted {
            run_id: "run-1".into(),
        },
        ActivityEvent::WorkflowFailed {
            run_id: "run-2".into(),
            error: "boom".into(),
        },
        ActivityEvent::WorkflowCancelled {
            run_id: "run-3".into(),
        },
    ];

    for event in workflow_events {
        let encoded = json!({"kind": "activity", "event": event});
        assert_eq!(encoded["kind"], "activity");
        assert!(
            encoded["event"]["type"]
                .as_str()
                .expect("workflow activity should keep its type tag")
                .starts_with("Workflow"),
            "workflow event should be preserved unchanged: {encoded:#?}"
        );
    }
}

/// S055 composability assertions: the first stdout line is one valid envelope
/// object, and every JSONL line is parseable by downstream tools such as `jq`
/// without special cases.
#[test]
fn jsonl_stdout_first_line_is_valid_envelope_for_line_oriented_consumers() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml(None));
    let provider = FakeProvider::scripted(
        vec![Ok(final_response("first line ready", 3, 6))],
        vec![vec![ProviderStreamEvent::TextDelta {
            text: "first".into(),
        }]],
    );

    let output = run_with_provider(
        jsonl_args(config.path_string(), "compose"),
        Box::new(provider),
    )
    .expect("jsonl run should return cli output");

    assert_eq!(output.exit_code, 0);
    let first_line = output
        .stdout_content
        .lines()
        .next()
        .expect("stdout should contain at least one JSONL line");
    let first: Value = serde_json::from_str(first_line)
        .unwrap_or_else(|error| panic!("first stdout line should parse as JSON: {error}"));
    assert!(
        matches!(first["kind"].as_str(), Some("activity" | "result")),
        "first line should be an envelope object: {first:#?}"
    );

    for line in parse_jsonl(&output.stdout_content) {
        assert!(
            matches!(line["kind"].as_str(), Some("activity" | "result")),
            "every parsed line should be a JSONL envelope object: {line:#?}"
        );
    }
}

/// S055 observability assertion: the CLI root span records
/// `simulacra.cli.output_format` as `jsonl` so operators can distinguish JSONL
/// runs from text runs.
#[test]
fn cli_root_span_records_jsonl_output_format_attribute() {
    let _guard = test_guard();
    let spans = capture_store();
    spans.lock().unwrap().clear();

    let config = TempConfig::write(&valid_config_toml(None));
    let output = run_with_provider(
        jsonl_args(config.path_string(), "trace format"),
        Box::new(FakeProvider::success("traced")),
    )
    .expect("jsonl run should return cli output");
    assert_eq!(output.exit_code, 0);

    let spans = spans.lock().unwrap().clone();
    assert!(
        spans.iter().any(|span| {
            span.name == "cli_run" && field_matches(span, "simulacra.cli.output_format", "jsonl")
        }),
        "cli_run span should record simulacra.cli.output_format=jsonl: {spans:#?}"
    );
}
