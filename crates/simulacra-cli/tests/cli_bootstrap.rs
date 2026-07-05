use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use simulacra_cli::{
    CliArgs, CliMode, ProviderKind, TracingBackend, bootstrap, infer_provider_kind, run,
    run_with_provider,
};
use simulacra_types::{
    FinishReason, Message, Provider, ProviderError, ProviderResponse, Role, TokenUsage,
    ToolDefinition,
};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
    parent: Option<String>,
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

fn capture_spans<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>) {
    let spans = capture_store();
    spans.lock().unwrap().clear();
    let result = f();
    let spans = spans.lock().unwrap().clone();
    (result, spans)
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
    // Install the capture subscriber before any guarded test calls `run()`.
    // Otherwise a non-capture run can win the process-global subscriber race
    // and later telemetry assertions become order-dependent.
    let _ = capture_store();
    guard
}

fn field_matches(span: &CapturedSpan, key: &str, expected: &str) -> bool {
    span.fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

fn unique_path(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "simulacra-cli-s013-{name}-{stamp}-{}.toml",
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

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: Option<&str>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.as_ref() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn with_env_var<T>(key: &'static str, value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _env = EnvGuard::set(key, value);
    f()
}

struct FakeProvider {
    responses: Mutex<Vec<Result<ProviderResponse, ProviderError>>>,
}

impl FakeProvider {
    fn success(text: &str) -> Self {
        Self {
            responses: Mutex::new(vec![Ok(final_response(text))]),
        }
    }

    fn failure(message: &str) -> Self {
        Self {
            responses: Mutex::new(vec![Err(ProviderError::Other(message.to_string()))]),
        }
    }
}

impl Provider for FakeProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut simulacra_types::ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut responses = self.responses.lock().map_err(|error| {
                ProviderError::Other(format!("poisoned fake provider: {error}"))
            })?;
            responses.remove(0)
        })
    }
}

struct RuntimeCheckingProvider;

impl Provider for RuntimeCheckingProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut simulacra_types::ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            assert_eq!(
                tokio::runtime::Handle::current().runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread,
                "S013 requires the CLI to initialize a tokio multi-thread runtime"
            );

            tokio::spawn(async {
                tracing::info_span!("provider_task", simulacra.operation.name = "provider_task")
                    .in_scope(|| {});
            })
            .await
            .map_err(|error| ProviderError::Other(error.to_string()))?;

            Ok(final_response("done"))
        })
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

fn args_with_task(config_path: String, task: &str) -> CliArgs {
    CliArgs {
        config_path,
        task: Some(task.to_string()),
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
    }
}

fn valid_config_toml(model: &str, task: Option<&str>) -> String {
    let task_line = task
        .map(|task| format!("task = {task:?}"))
        .unwrap_or_default();

    format!(
        r#"[project]
name = "simulacra-cli-spec"

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

#[test]
fn task_flag_hello_parses_and_defaults_to_headless_mode() {
    let _guard = test_guard();
    let args = CliArgs::try_parse_from(["simulacra", "--task", "hello"])
        .expect("`simulacra --task hello` should parse");

    assert_eq!(args.task.as_deref(), Some("hello"));
    assert_eq!(args.mode, Some(CliMode::Headless));
}

#[test]
fn custom_config_path_is_parsed_from_the_flag() {
    let _guard = test_guard();
    let args = CliArgs::try_parse_from(["simulacra", "--config", "custom.toml", "--task", "x"])
        .expect("custom config path should parse");

    assert_eq!(args.config_path, "custom.toml");
    assert_eq!(args.task.as_deref(), Some("x"));
}

#[test]
fn interactive_mode_starts_successfully_after_phase_two() {
    let _guard = test_guard();
    let config = TempConfig::missing();
    let output = run(CliArgs {
        config_path: config.path_string(),
        task: Some("say hello".into()),
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
        "interactive mode should no longer be blocked: {:?}",
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
fn missing_task_in_args_and_config_exits_with_a_clear_error() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", None));

    let output = run(CliArgs {
        config_path: config.path_string(),
        task: None,
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
    .expect("missing task should be rendered as cli output");

    assert_eq!(output.exit_code, 1);
    assert!(
        output
            .stderr_content
            .contains("no task specified. Use --task or set [task].task in config."),
        "stderr should explain how to provide a task: {:?}",
        output.stderr_content
    );
}

#[test]
fn valid_config_is_parsed_and_entry_agent_is_resolved() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml(
        "claude-sonnet-4-20250514",
        Some("review the code"),
    ));

    let bootstrap = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: None,
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
    .expect("bootstrap should parse a valid config");

    assert_eq!(bootstrap.entry_agent, "default");
    assert_eq!(bootstrap.task, "review the code");
}

#[test]
fn cli_task_overrides_the_task_from_config() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml(
        "claude-sonnet-4-20250514",
        Some("from config"),
    ));

    let bootstrap = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: Some("from cli".into()),
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
    .expect("bootstrap should prefer `--task` over `[task].task`");

    assert_eq!(bootstrap.task, "from cli");
}

#[test]
fn missing_config_with_task_uses_default_config_and_simulacra_model_or_fallback() {
    let _guard = test_guard();
    let missing = TempConfig::missing();

    with_env_var("SIMULACRA_MODEL", Some("gpt-5.4"), || {
        let bootstrap = bootstrap(&args_with_task(missing.path_string(), "ad hoc task"))
            .expect("missing config with task should synthesize an implicit config");

        assert_eq!(bootstrap.config.project.name, "simulacra-adhoc");
        assert_eq!(bootstrap.entry_agent, "default");
        assert_eq!(bootstrap.model, "gpt-5.4");
        assert!(bootstrap.capability_token.shell);
        assert!(bootstrap.capability_token.javascript);
        assert_eq!(
            bootstrap.capability_token.paths_read,
            vec![simulacra_types::PathPattern("/**".into())]
        );
        assert_eq!(
            bootstrap.capability_token.paths_write,
            vec![simulacra_types::PathPattern("/**".into())]
        );
        assert_eq!(bootstrap.resource_budget.max_turns, 50);
        assert_eq!(bootstrap.resource_budget.max_tokens, 200_000);
        assert!(
            bootstrap.config.mcp.is_none(),
            "implicit config should not create MCP servers"
        );
    });

    with_env_var("SIMULACRA_MODEL", None, || {
        let bootstrap = bootstrap(&args_with_task(missing.path_string(), "ad hoc task"))
            .expect("missing config with task should still work without SIMULACRA_MODEL");

        assert_eq!(bootstrap.model, "claude-sonnet-4-6");
    });
}

#[test]
fn invalid_toml_config_exits_with_a_parse_error_message() {
    let _guard = test_guard();
    let config = TempConfig::write("this = [ definitely not valid toml");

    let output = run(CliArgs {
        config_path: config.path_string(),
        task: Some("hello".into()),
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
    .expect("parse failures should become cli output");

    assert_eq!(output.exit_code, 1);
    assert!(
        output.stderr_content.contains("failed to parse TOML"),
        "stderr should include the config parse failure: {:?}",
        output.stderr_content
    );
}

#[test]
fn capability_token_is_built_from_the_agent_capabilities_section() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let bootstrap = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: None,
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
    .expect("bootstrap should resolve capabilities");

    assert!(bootstrap.capability_token.shell);
    assert!(bootstrap.capability_token.javascript);
    assert_eq!(
        bootstrap.capability_token.paths_read,
        vec![simulacra_types::PathPattern("/workspace/**".into())]
    );
    assert_eq!(
        bootstrap.capability_token.paths_write,
        vec![simulacra_types::PathPattern("/workspace/**".into())]
    );
}

#[test]
fn resource_budget_is_built_from_agent_type_limits() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let bootstrap = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: None,
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
    .expect("bootstrap should resolve resource budget");

    assert_eq!(bootstrap.resource_budget.max_turns, 7);
    assert_eq!(bootstrap.resource_budget.max_tokens, 4321);
}

#[test]
fn bootstrap_creates_a_vfs_and_preseeds_workspace_task_md() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml(
        "claude-sonnet-4-20250514",
        Some("draft a plan"),
    ));

    let bootstrap = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: None,
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
    .expect("bootstrap should prepare the workspace vfs");

    let task_md = bootstrap
        .vfs
        .read("/workspace/task.md")
        .expect("/workspace/task.md should exist");
    assert_eq!(
        String::from_utf8(task_md).expect("task file should be utf-8"),
        "draft a plan"
    );
}

#[test]
fn bootstrap_registers_the_six_builtin_tools_from_s012() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let bootstrap = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: None,
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
    .expect("bootstrap should register built-in tools");

    let mut tool_names: Vec<&str> = bootstrap
        .tool_definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();
    tool_names.sort_unstable();

    #[cfg(not(feature = "python"))]
    {
        assert_eq!(tool_names.len(), 6);
        assert_eq!(
            tool_names,
            vec![
                "apply_patch",
                "file_read",
                "file_write",
                "js_exec",
                "list_dir",
                "shell_exec",
            ]
        );
    }
    #[cfg(feature = "python")]
    {
        assert_eq!(tool_names.len(), 7);
        assert_eq!(
            tool_names,
            vec![
                "apply_patch",
                "file_read",
                "file_write",
                "js_exec",
                "list_dir",
                "py_exec",
                "shell_exec",
            ]
        );
    }
}

#[test]
fn provider_is_constructed_from_the_model_string_in_config() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("gpt-5.4", Some("task")));

    let bootstrap = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: None,
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
    .expect("bootstrap should carry the configured model into provider construction");

    assert_eq!(bootstrap.model, "gpt-5.4");
}

#[test]
fn provider_selection_infers_anthropic_openai_and_ollama_from_model_prefixes() {
    let _guard = test_guard();
    assert_eq!(
        infer_provider_kind("claude-sonnet-4-20250514").expect("claude models should map"),
        ProviderKind::Anthropic
    );
    assert_eq!(
        infer_provider_kind("gpt-5.4").expect("gpt models should map"),
        ProviderKind::OpenAI
    );
    assert_eq!(
        infer_provider_kind("o1-preview").expect("o1 models should map"),
        ProviderKind::OpenAI
    );
    assert_eq!(
        infer_provider_kind("o3-mini").expect("o3 models should map"),
        ProviderKind::OpenAI
    );
    assert_eq!(
        infer_provider_kind("ollama:llama3.2").expect("ollama models should map"),
        ProviderKind::Ollama
    );
}

#[test]
fn headless_mode_prints_the_final_response_to_stdout_without_decoration() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let output = run_with_provider(
        CliArgs {
            config_path: config.path_string(),
            task: None,
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
        },
        Box::new(FakeProvider::success("final answer")),
    )
    .expect("headless success should be returned as cli output");

    assert_eq!(output.stdout_content, "final answer");
}

#[test]
fn log_output_goes_to_stderr_and_not_stdout() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let output = run_with_provider(
        CliArgs {
            config_path: config.path_string(),
            task: None,
            mode: Some(CliMode::Headless),
            verbose: true,
            otlp_endpoint: None,
            session: None,
            model: None,
            max_turns: None,
            max_tokens: None,
            max_cost: None,
            no_catalog: false,
            output_format: simulacra_cli::OutputFormat::Text,
        },
        Box::new(FakeProvider::success("final answer")),
    )
    .expect("successful runs should return captured output");

    assert_eq!(output.stdout_content, "final answer");
    assert!(
        output.stderr_content.contains("DEBUG"),
        "stderr should contain tracing output when verbose mode is enabled: {:?}",
        output.stderr_content
    );
    assert!(
        !output.stdout_content.contains("DEBUG"),
        "stdout must remain clean for pipeline composition: {:?}",
        output.stdout_content
    );
}

#[test]
fn successful_run_exits_with_code_zero() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let output = run_with_provider(
        CliArgs {
            config_path: config.path_string(),
            task: None,
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
        },
        Box::new(FakeProvider::success("ok")),
    )
    .expect("successful runs should surface cli output");

    assert_eq!(output.exit_code, 0);
}

#[test]
fn failed_run_exits_with_code_one_and_prints_a_human_readable_error() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let output = run_with_provider(
        CliArgs {
            config_path: config.path_string(),
            task: None,
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
        },
        Box::new(FakeProvider::failure("provider boom")),
    )
    .expect("provider failures should still become cli output");

    assert_eq!(output.exit_code, 1);
    assert!(
        output.stderr_content.contains("provider boom"),
        "stderr should contain the provider failure message: {:?}",
        output.stderr_content
    );
}

#[test]
fn otlp_endpoint_selects_the_otlp_tracing_backend() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let bootstrap = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: None,
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: Some("http://127.0.0.1:4318".into()),
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: false,
        output_format: simulacra_cli::OutputFormat::Text,
    })
    .expect("bootstrap should resolve tracing configuration");

    assert_eq!(bootstrap.tracing_plan.backend, TracingBackend::Otlp);
    assert_eq!(
        bootstrap.tracing_plan.otlp_endpoint.as_deref(),
        Some("http://127.0.0.1:4318")
    );
}

#[test]
fn without_otlp_endpoint_tracing_uses_the_stderr_fmt_subscriber() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let bootstrap = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: None,
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
    .expect("bootstrap should resolve tracing configuration");

    assert_eq!(bootstrap.tracing_plan.backend, TracingBackend::StderrFmt);
}

#[test]
fn verbose_mode_enables_debug_level_output() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let bootstrap = bootstrap(&CliArgs {
        config_path: config.path_string(),
        task: None,
        mode: Some(CliMode::Headless),
        verbose: true,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: false,
        output_format: simulacra_cli::OutputFormat::Text,
    })
    .expect("bootstrap should resolve tracing verbosity");

    assert_eq!(bootstrap.tracing_plan.level, "DEBUG");
}

#[test]
fn cli_startup_emits_a_root_span_with_task_and_config_metadata() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));
    let long_task = "x".repeat(140);

    let (_, spans) = capture_spans(|| {
        run_with_provider(
            CliArgs {
                config_path: config.path_string(),
                task: Some(long_task.clone()),
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
            },
            Box::new(FakeProvider::success("done")),
        )
        .expect("run should succeed so spans can be inspected");
    });

    let root = spans
        .iter()
        .find(|span| field_matches(span, "simulacra.operation.name", "cli_run"))
        .expect("expected a cli root span");

    assert_eq!(root.name, "cli_run");
    assert_eq!(
        root.fields.get("simulacra.config.path"),
        Some(&config.path_string())
    );
    assert_eq!(
        root.fields
            .get("simulacra.task")
            .map(|value| value.trim_matches('"').len()),
        Some(100)
    );
}

#[test]
fn agent_loop_span_is_a_child_of_the_cli_root_span() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let (_, spans) = capture_spans(|| {
        run_with_provider(
            CliArgs {
                config_path: config.path_string(),
                task: None,
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
            },
            Box::new(FakeProvider::success("done")),
        )
        .expect("run should succeed so spans can be inspected");
    });

    let agent_loop = spans
        .iter()
        .find(|span| field_matches(span, "gen_ai.operation.name", "invoke_agent"))
        .expect("expected the agent loop span emitted by simulacra-runtime");

    assert_eq!(agent_loop.parent.as_deref(), Some("cli_run"));
}

#[test]
fn cli_shutdown_flushes_the_otlp_exporter_before_exit() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let output = run_with_provider(
        CliArgs {
            config_path: config.path_string(),
            task: None,
            mode: Some(CliMode::Headless),
            verbose: false,
            otlp_endpoint: Some("http://127.0.0.1:4318".into()),
            session: None,
            model: None,
            max_turns: None,
            max_tokens: None,
            max_cost: None,
            no_catalog: false,
            output_format: simulacra_cli::OutputFormat::Text,
        },
        Box::new(FakeProvider::success("done")),
    )
    .expect("successful OTLP runs should return cli output");

    assert!(
        output.telemetry_flushed,
        "cli shutdown should flush OTLP telemetry before returning"
    );
}

#[test]
fn headless_run_uses_a_tokio_multi_thread_runtime_and_captures_spawned_work() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let (_, spans) = capture_spans(|| {
        run_with_provider(
            CliArgs {
                config_path: config.path_string(),
                task: None,
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
            },
            Box::new(RuntimeCheckingProvider),
        )
        .expect("runtime-checking provider should complete");
    });

    assert!(
        spans
            .iter()
            .any(|span| field_matches(span, "simulacra.operation.name", "provider_task")),
        "spawned provider work should emit spans that are still captured by the CLI harness"
    );
}

// ---------------------------------------------------------------------------
// S038 — CLI memory wiring
// ---------------------------------------------------------------------------

// TODO S038: requires a CLI memory bootstrap seam that records BackgroundEmbedder
// spawn attempts so this integration test can assert the zero-spawn path.
#[test]
#[ignore]
fn memory_absent_does_not_spawn_a_background_embedder() {}

#[test]
fn memory_absent_does_not_register_memory_tools() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let bootstrap = bootstrap(&args_with_task(config.path_string(), "task"))
        .expect("bootstrap should succeed without a [memory] section");
    let tool_names: Vec<String> = bootstrap
        .tool_definitions
        .into_iter()
        .map(|tool| tool.name)
        .collect();

    assert!(
        !tool_names.iter().any(|name| name == "semantic_search"),
        "semantic_search must not be registered when [memory] is absent: {tool_names:?}"
    );
    assert!(
        !tool_names.iter().any(|name| name == "memory_read_chunk"),
        "memory_read_chunk must not be registered when [memory] is absent: {tool_names:?}"
    );
}

#[test]
fn memory_absent_vfs_write_to_var_memory_returns_not_found() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let bootstrap = bootstrap(&args_with_task(config.path_string(), "task"))
        .expect("bootstrap should succeed without a [memory] section");
    let err =
        simulacra_types::VirtualFs::write(&bootstrap.vfs, "/var/memory/foo.md", b"hello world")
            .expect_err("without memory wiring, /var/memory writes should fail");

    assert!(
        err.to_string().to_ascii_lowercase().contains("not found"),
        "expected /var/memory write to fail with NotFound, got: {err}"
    );
}

#[test]
fn memory_absent_emits_no_memory_bootstrap_span() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let (_, spans) = capture_spans(|| {
        run_with_provider(
            args_with_task(config.path_string(), "task"),
            Box::new(FakeProvider::success("done")),
        )
        .expect("run should complete so spans can be inspected");
    });

    assert!(
        !spans.iter().any(|span| span.name == "memory_bootstrap"),
        "memory_bootstrap must be absent when [memory] is absent: {spans:?}"
    );
}

// TODO S038: requires a CLI log-capture seam for bootstrap-time warn! output so
// this test can assert the missing-[memory] warning line.
#[test]
#[ignore]
fn memory_absent_warns_when_the_entry_agent_claims_memory() {}

#[test]
fn memory_enabled_creates_the_configured_directory_if_absent() {
    let _guard = test_guard();
    let memory_root = unique_path("memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let config = TempConfig::write(&format!(
        r#"{} 

[memory]
dir = {:?}
tenant = "cli"

[agent_types.default.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("task")),
        memory_root.display().to_string(),
    ));

    assert!(
        !memory_root.exists(),
        "test precondition violated: configured memory dir should start absent"
    );

    let output = run_with_provider(
        args_with_task(config.path_string(), "task"),
        Box::new(FakeProvider::success("done")),
    )
    .expect("run should return cli output");

    assert_eq!(
        output.exit_code, 0,
        "run should succeed once memory is wired"
    );
    assert!(
        memory_root.exists(),
        "memory-enabled CLI bootstrap must create the configured dir: {}",
        memory_root.display()
    );
}

#[test]
fn memory_enabled_wraps_the_vfs_and_persists_var_memory_writes_into_the_tenant_db() {
    let _guard = test_guard();
    let memory_root = unique_path("memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let tenant_db = memory_root.join("memory").join("cli.db");
    let config = TempConfig::write(&format!(
        r#"{} 

[memory]
dir = {:?}
tenant = "cli"

[agent_types.default.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("task")),
        memory_root.display().to_string(),
    ));

    let bootstrap = bootstrap(&args_with_task(config.path_string(), "task"))
        .expect("bootstrap should succeed with memory enabled");
    simulacra_types::VirtualFs::write(
        &bootstrap.vfs,
        "/var/memory/self/note.md",
        b"launch checklist: verify rollout",
    )
    .expect("memory-enabled VFS should accept writes under /var/memory/self");

    assert!(
        tenant_db.exists(),
        "memory write should persist into the configured tenant DB: {}",
        tenant_db.display()
    );
}

#[test]
fn memory_enabled_registers_memory_tools_via_real_tool_invocation() {
    let _guard = test_guard();
    let memory_root = unique_path("memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let config = TempConfig::write(&format!(
        r#"{} 

[memory]
dir = {:?}
tenant = "cli"

[agent_types.default.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("task")),
        memory_root.display().to_string(),
    ));

    let (_, spans) = capture_spans(|| {
        run_with_provider(
            args_with_task(config.path_string(), "task"),
            Box::new(FakeProvider {
                responses: Mutex::new(vec![
                    Ok(ProviderResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: String::new(),
                            tool_calls: vec![simulacra_types::ToolCallMessage {
                                id: "tc-memory-search".into(),
                                name: "semantic_search".into(),
                                arguments: serde_json::json!({
                                    "query": "launch checklist",
                                    "scope": "/var/memory/self/",
                                    "k": 3
                                }),
                            }],
                            tool_call_id: None,
                        },
                        token_usage: TokenUsage {
                            input_tokens: 20,
                            output_tokens: 10,
                        },
                        finish_reason: FinishReason::ToolUse,
                        provider_response_id: Some("resp-memory-search".into()),
                        model: "claude-sonnet-4-20250514".into(),
                    }),
                    Ok(final_response("done")),
                ]),
            }),
        )
        .expect("run should complete so tool-call spans can be inspected");
    });

    assert!(
        spans.iter().any(|span| span.name == "memory_search"),
        "successful semantic_search invocation should emit a memory_search span: {spans:?}"
    );
}

// TODO S038: requires a CLI seam exposing BackgroundEmbedder spawn count after
// the tokio runtime is created so this test can assert exactly-one spawn.
#[test]
#[ignore]
fn memory_enabled_spawns_the_background_embedder_exactly_once() {}

// TODO S038: requires a CliBootstrap accessor exposing the RRWB Arc used by the
// VFS and the memory tool handles so pointer equality can be asserted.
#[test]
#[ignore]
fn memory_enabled_shares_rrwb_arc_between_memory_store_fs_and_memory_tools() {}

#[test]
fn memory_enabled_persists_writes_across_two_run_with_provider_calls() {
    let _guard = test_guard();
    let memory_root = unique_path("memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let tenant_db = memory_root.join("memory").join("cli.db");
    let config = TempConfig::write(&format!(
        r#"{} 

[memory]
dir = {:?}
tenant = "cli"

[agent_types.default.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("task")),
        memory_root.display().to_string(),
    ));

    // BOTH runs must live inside capture_spans so the capture subscriber
    // wins the OnceLock race against run_with_provider's bootstrap
    // subscriber. Running the first call outside capture_spans panics in
    // isolation (and in some test orderings) because the cli bootstrap
    // installs a non-capture subscriber first and the later capture_store
    // init call fails its set_global_default.
    let (first, spans) = capture_spans(|| {
        let first = run_with_provider(
            args_with_task(config.path_string(), "task"),
            Box::new(FakeProvider {
                responses: Mutex::new(vec![
                    Ok(ProviderResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: String::new(),
                            tool_calls: vec![simulacra_types::ToolCallMessage {
                                id: "tc-memory-write".into(),
                                name: "file_write".into(),
                                arguments: serde_json::json!({
                                    "path": "/var/memory/self/note.md",
                                    "content": "note about launch checklist and rollback drill"
                                }),
                            }],
                            tool_call_id: None,
                        },
                        token_usage: TokenUsage {
                            input_tokens: 20,
                            output_tokens: 10,
                        },
                        finish_reason: FinishReason::ToolUse,
                        provider_response_id: Some("resp-memory-write".into()),
                        model: "claude-sonnet-4-20250514".into(),
                    }),
                    Ok(final_response("stored")),
                ]),
            }),
        )
        .expect("first run should return cli output");

        assert_eq!(first.exit_code, 0, "first run should complete successfully");
        assert!(
            tenant_db.exists(),
            "first run should create the configured tenant DB: {}",
            tenant_db.display()
        );

        first
    });
    let _ = first;
    let _ = spans;

    let (_, spans) = capture_spans(|| {
        run_with_provider(
            args_with_task(config.path_string(), "task"),
            Box::new(FakeProvider {
                responses: Mutex::new(vec![
                    Ok(ProviderResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: String::new(),
                            tool_calls: vec![simulacra_types::ToolCallMessage {
                                id: "tc-memory-search".into(),
                                name: "semantic_search".into(),
                                arguments: serde_json::json!({
                                    "query": "launch checklist",
                                    "scope": "/var/memory/self/",
                                    "k": 3
                                }),
                            }],
                            tool_call_id: None,
                        },
                        token_usage: TokenUsage {
                            input_tokens: 20,
                            output_tokens: 10,
                        },
                        finish_reason: FinishReason::ToolUse,
                        provider_response_id: Some("resp-memory-search".into()),
                        model: "claude-sonnet-4-20250514".into(),
                    }),
                    Ok(final_response("found")),
                ]),
            }),
        )
        .expect("second run should return cli output");
    });

    let search_span = spans
        .iter()
        .find(|span| span.name == "memory_search")
        .expect("second run should invoke semantic_search against persisted memory");
    let hit_count = search_span
        .fields
        .get("memory.hit_count")
        .and_then(|value| value.trim_matches('"').parse::<usize>().ok())
        .unwrap_or(0);

    assert!(
        hit_count > 0,
        "second run should find the prior run's memory write, got hit_count={hit_count} with span {search_span:?}"
    );
}

#[test]
fn memory_disabled_does_not_install_memory_store_fs() {
    let _guard = test_guard();
    let memory_root = unique_path("memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let config = TempConfig::write(&format!(
        r#"{} 

[memory]
dir = {:?}
tenant = "cli"

[agent_types.default.capabilities.memory]
enabled = false
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("task")),
        memory_root.display().to_string(),
    ));

    let bootstrap = bootstrap(&args_with_task(config.path_string(), "task")).expect(
        "bootstrap should succeed with [memory] configured but disabled on the entry agent",
    );
    let err =
        simulacra_types::VirtualFs::write(&bootstrap.vfs, "/var/memory/self/note.md", b"hello")
            .expect_err("disabled entry-agent memory should reject /var/memory writes");
    let message = err.to_string().to_ascii_lowercase();

    assert!(
        message.contains("not found") || message.contains("denied"),
        "expected disabled memory write to fail with NotFound or denied, got: {err}"
    );
}

#[test]
fn memory_disabled_does_not_register_memory_tools() {
    let _guard = test_guard();
    let memory_root = unique_path("memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let config = TempConfig::write(&format!(
        r#"{} 

[memory]
dir = {:?}
tenant = "cli"

[agent_types.default.capabilities.memory]
enabled = false
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("task")),
        memory_root.display().to_string(),
    ));

    let bootstrap = bootstrap(&args_with_task(config.path_string(), "task")).expect(
        "bootstrap should succeed with [memory] configured but disabled on the entry agent",
    );
    let tool_names: Vec<String> = bootstrap
        .tool_definitions
        .into_iter()
        .map(|tool| tool.name)
        .collect();

    assert!(
        !tool_names.iter().any(|name| name == "semantic_search"),
        "semantic_search must not be registered when entry-agent memory is disabled: {tool_names:?}"
    );
    assert!(
        !tool_names.iter().any(|name| name == "memory_read_chunk"),
        "memory_read_chunk must not be registered when entry-agent memory is disabled: {tool_names:?}"
    );
}

// TODO S038: requires a CLI memory bootstrap seam that exposes whether
// BackgroundEmbedder::spawn was attempted on the disabled path.
#[test]
#[ignore]
fn memory_disabled_does_not_spawn_a_background_embedder() {}

// TODO S038: requires a CLI log-capture seam for bootstrap-time warn! output so
// this test can assert the configured-but-disabled warning line.
#[test]
#[ignore]
fn memory_disabled_logs_a_warning_when_memory_is_configured_but_unused() {}

#[test]
fn memory_disabled_still_denies_var_memory_even_with_global_paths_write() {
    let _guard = test_guard();
    let memory_root = unique_path("memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let config = TempConfig::write(&format!(
        r#"[project]
name = "simulacra-cli-spec"

[agent_types.default]
model = "claude-sonnet-4-20250514"
max_turns = 7
max_tokens = 4321

[agent_types.default.capabilities]
shell = true
javascript = true
paths_read = ["/workspace/**"]
paths_write = ["/**"]

[agent_types.default.capabilities.memory]
enabled = false
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]

[task]
entry_agent = "default"
task = "task"

[memory]
dir = {:?}
tenant = "cli"
"#,
        memory_root.display().to_string(),
    ));

    let bootstrap = bootstrap(&args_with_task(config.path_string(), "task"))
        .expect("bootstrap should succeed with [memory] configured");
    let err = bootstrap
        .capability_token
        .check_path_write("/var/memory/foo.md")
        .expect_err("global paths_write must not bypass memory.enabled=false");

    assert!(
        err.reason.contains("MemoryCapability.write_scopes")
            || err.reason.contains("memory write denied"),
        "unexpected denial reason: {}",
        err.reason
    );
}

#[test]
fn memory_enabled_allows_var_memory_self_writes_with_write_scopes_even_when_paths_write_is_workspace_only()
 {
    let _guard = test_guard();
    let memory_root = unique_path("memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let tenant_db = memory_root.join("memory").join("cli.db");
    let config = TempConfig::write(&format!(
        r#"[project]
name = "simulacra-cli-spec"

[agent_types.default]
model = "claude-sonnet-4-20250514"
max_turns = 7
max_tokens = 4321

[agent_types.default.capabilities]
shell = true
javascript = true
paths_read = ["/workspace/**"]
paths_write = ["/workspace/**"]

[agent_types.default.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]

[task]
entry_agent = "default"
task = "task"

[memory]
dir = {:?}
tenant = "cli"
"#,
        memory_root.display().to_string(),
    ));

    let bootstrap = bootstrap(&args_with_task(config.path_string(), "task"))
        .expect("bootstrap should succeed with memory enabled");
    bootstrap
        .capability_token
        .check_path_write("/var/memory/self/foo.md")
        .expect("memory write_scopes should grant /var/memory/self");
    simulacra_types::VirtualFs::write(&bootstrap.vfs, "/var/memory/self/foo.md", b"ready")
        .expect("VFS should accept /var/memory/self writes once memory is wired");

    assert!(
        tenant_db.exists(),
        "granted memory write should persist into the configured tenant DB: {}",
        tenant_db.display()
    );
}

// TODO S038: requires a CLI seam exposing the BackgroundEmbedder shutdown call
// on the normal-success path.
#[test]
#[ignore]
fn lifecycle_success_path_calls_background_embedder_shutdown() {}

// TODO S038: requires a CLI seam exposing the BackgroundEmbedder shutdown call
// on the agent-loop error path.
#[test]
#[ignore]
fn lifecycle_error_path_calls_background_embedder_shutdown() {}

// TODO S038: requires a seam that reports both the agent-loop result and a
// BackgroundEmbedder::shutdown error from run_booted.
#[test]
#[ignore]
fn lifecycle_reports_shutdown_errors_alongside_agent_loop_errors() {}

// TODO S038: requires a runtime task-leak seam so the test can assert no
// memory-related background tokio tasks survive run_booted returning.
#[test]
#[ignore]
fn lifecycle_leaves_no_background_tasks_running_after_run_booted_returns() {}

#[test]
fn telemetry_emits_memory_bootstrap_span_when_memory_is_wired() {
    let _guard = test_guard();
    let memory_root = unique_path("memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let memory_root_str = memory_root.display().to_string();
    let config = TempConfig::write(&format!(
        r#"{} 

[memory]
dir = {:?}
tenant = "cli"

[agent_types.default.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("task")),
        memory_root_str,
    ));

    let (_, spans) = capture_spans(|| {
        run_with_provider(
            args_with_task(config.path_string(), "task"),
            Box::new(FakeProvider::success("done")),
        )
        .expect("run should complete so spans can be inspected");
    });

    let span = spans
        .iter()
        .find(|span| span.name == "memory_bootstrap")
        .expect("memory-enabled bootstrap should emit a memory_bootstrap span");

    assert_eq!(span.parent.as_deref(), Some("cli_run"));
    assert!(field_matches(
        span,
        "simulacra.memory.dir",
        &memory_root_str
    ));
    assert!(field_matches(span, "simulacra.memory.tenant", "cli"));
    assert!(field_matches(
        span,
        "simulacra.memory.entry_agent_enabled",
        "true"
    ));
    assert!(field_matches(span, "simulacra.memory.outcome", "wired"));
    assert!(
        span.fields.contains_key("simulacra.memory.embedder_id"),
        "memory_bootstrap should record simulacra.memory.embedder_id: {span:?}"
    );
    assert!(
        span.fields.contains_key("simulacra.memory.embedder_dim"),
        "memory_bootstrap should record simulacra.memory.embedder_dim: {span:?}"
    );
}

#[test]
fn telemetry_emits_memory_bootstrap_span_with_skipped_disabled_outcome() {
    let _guard = test_guard();
    let memory_root = unique_path("memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let memory_root_str = memory_root.display().to_string();
    let config = TempConfig::write(&format!(
        r#"{} 

[memory]
dir = {:?}
tenant = "cli"

[agent_types.default.capabilities.memory]
enabled = false
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("task")),
        memory_root_str,
    ));

    let (_, spans) = capture_spans(|| {
        run_with_provider(
            args_with_task(config.path_string(), "task"),
            Box::new(FakeProvider::success("done")),
        )
        .expect("run should complete so spans can be inspected");
    });

    let span = spans
        .iter()
        .find(|span| span.name == "memory_bootstrap")
        .expect("configured-but-disabled memory should still emit a memory_bootstrap span");

    assert_eq!(span.parent.as_deref(), Some("cli_run"));
    assert!(field_matches(
        span,
        "simulacra.memory.dir",
        &memory_root_str
    ));
    assert!(field_matches(span, "simulacra.memory.tenant", "cli"));
    assert!(field_matches(
        span,
        "simulacra.memory.entry_agent_enabled",
        "false"
    ));
    assert!(field_matches(
        span,
        "simulacra.memory.outcome",
        "skipped_disabled_for_entry_agent"
    ));
}

#[test]
fn telemetry_emits_no_memory_bootstrap_span_when_memory_is_absent() {
    let _guard = test_guard();
    let config = TempConfig::write(&valid_config_toml("claude-sonnet-4-20250514", Some("task")));

    let (_, spans) = capture_spans(|| {
        run_with_provider(
            args_with_task(config.path_string(), "task"),
            Box::new(FakeProvider::success("done")),
        )
        .expect("run should complete so spans can be inspected");
    });

    assert!(
        !spans.iter().any(|span| span.name == "memory_bootstrap"),
        "memory_bootstrap must be absent when no [memory] section exists: {spans:?}"
    );
}

#[test]
fn negative_memory_dir_creation_failure_returns_nonzero_exit_and_human_readable_stderr() {
    let _guard = test_guard();
    let config = TempConfig::write(&format!(
        r#"{} 

[memory]
dir = "/dev/null/cannot-create"
tenant = "cli"

[agent_types.default.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("task")),
    ));

    let output = run_with_provider(
        args_with_task(config.path_string(), "task"),
        Box::new(FakeProvider::success("done")),
    )
    .expect("startup failures should still return cli output");

    assert_ne!(output.exit_code, 0);
    assert!(
        output.stderr_content.contains("cannot create memory dir"),
        "expected memory-dir creation failure in stderr, got: {:?}",
        output.stderr_content
    );
}

#[test]
fn negative_invalid_memory_tenant_returns_nonzero_exit_and_stderr_message() {
    let _guard = test_guard();
    let memory_root = unique_path("memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let config = TempConfig::write(&format!(
        r#"{} 

[memory]
dir = {:?}
tenant = "INVALID UPPER"

[agent_types.default.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("task")),
        memory_root.display().to_string(),
    ));

    let output = run_with_provider(
        args_with_task(config.path_string(), "task"),
        Box::new(FakeProvider::success("done")),
    )
    .expect("startup failures should still return cli output");

    assert_ne!(output.exit_code, 0);
    assert!(
        output.stderr_content.contains("invalid tenant id"),
        "expected invalid-tenant error in stderr, got: {:?}",
        output.stderr_content
    );
}

#[test]
fn negative_corrupt_memory_sqlite_returns_nonzero_exit_and_stderr_message() {
    let _guard = test_guard();
    let memory_root = unique_path("corrupt-memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let corrupt_dir = memory_root.join("memory");
    fs::create_dir_all(&corrupt_dir).expect("memory dir should be created");
    fs::write(corrupt_dir.join("cli.db"), b"not a sqlite database")
        .expect("corrupt tenant db should be written");
    let config = TempConfig::write(&format!(
        r#"{} 

[memory]
dir = {:?}
tenant = "cli"

[agent_types.default.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("task")),
        memory_root.display().to_string(),
    ));

    let output = run_with_provider(
        args_with_task(config.path_string(), "task"),
        Box::new(FakeProvider::success("done")),
    )
    .expect("startup failures should still return cli output");

    assert_ne!(output.exit_code, 0);
    // Corrupt sqlite file now surfaces at fingerprint-read time via
    // the S037 on_model_change policy dispatch, which runs before
    // ensure_tenant. Either path should mention "memory:" plus a
    // sqlite error indicating the file isn't a valid database.
    assert!(
        output.stderr_content.contains("memory:")
            && (output
                .stderr_content
                .to_lowercase()
                .contains("not a database")
                || output.stderr_content.contains("ensure_tenant failed")),
        "expected memory bootstrap failure in stderr, got: {:?}",
        output.stderr_content
    );
}

#[test]
fn negative_memory_startup_failures_use_the_existing_cli_config_error_exit_code() {
    let _guard = test_guard();
    let memory_root = unique_path("memory-root");
    let _ = fs::remove_file(&memory_root);
    let _ = fs::remove_dir_all(&memory_root);
    let config = TempConfig::write(&format!(
        r#"{} 

[memory]
dir = {:?}
tenant = "INVALID UPPER"

[agent_types.default.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self/"]
write_scopes = ["/var/memory/self/"]
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("task")),
        memory_root.display().to_string(),
    ));

    let output = run_with_provider(
        args_with_task(config.path_string(), "task"),
        Box::new(FakeProvider::success("done")),
    )
    .expect("startup failures should still return cli output");

    assert_eq!(
        output.exit_code, 1,
        "memory startup failures should use the existing config-fatal exit code"
    );
}

// ── S039: vfs_write hook config wiring ──────────────────────────────────

/// S039 BLOCKER 2 fix: a `[[hooks.vfs_write]]` entry in `simulacra.toml` must be
/// reachable through the bootstrapped pipeline. Pre-fix, the entry was
/// silently dropped because `HooksConfig` and the CLI bootstrap loop only
/// knew about `tool_call` / `llm` / `spawn` / `http_request`.
#[test]
fn s039_bootstrap_registers_vfs_write_hook_from_config() {
    let _guard = test_guard();
    // Use the in-tree pass-through fixture as a real, loadable JS module.
    // Resolved relative to the workspace root because `bootstrap` runs in
    // the same CWD as `cargo test` (workspace root).
    let hook_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ parent")
        .join("simulacra-hooks")
        .join("fixtures")
        .join("pass-through.js");
    let hook_path_str = hook_path.to_string_lossy().into_owned();
    let config = TempConfig::write(&format!(
        r#"{}

[[hooks.vfs_write]]
name = "vfs-audit-test"
runtime = "js"
module = {hook_path_str:?}
"#,
        valid_config_toml("claude-sonnet-4-20250514", Some("noop")),
    ));

    let bootstrap = bootstrap(&args_with_task(config.path_string(), "noop"))
        .expect("bootstrap should succeed when [[hooks.vfs_write]] is configured");

    let names = bootstrap.hook_names("vfs_write");
    assert_eq!(
        names,
        vec!["vfs-audit-test".to_string()],
        "expected the configured vfs_write hook to be registered, got {names:?}"
    );
    // Sibling sanity: existing op chains still default to empty without their
    // own [[hooks.<op>]] entries — we did not accidentally cross-pollinate.
    assert!(
        bootstrap.hook_names("tool_call").is_empty(),
        "tool_call chain should be empty, got {:?}",
        bootstrap.hook_names("tool_call")
    );
}
