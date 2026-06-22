use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rust_decimal::Decimal;
use serde_json::Value;
use simulacra_cli::{CliArgs, CliMode, bootstrap, run_with_provider};
use simulacra_config::SimulacraConfig;
use simulacra_runtime::InMemoryJournalStorage;
use simulacra_sandbox::AgentCell;
use simulacra_types::{
    CapabilityToken, FinishReason, JournalStorage, Message, PathPattern, Provider, ProviderError,
    ProviderResponse, ResourceBudget, Role, TokenUsage, ToolDefinition, VirtualFs,
};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
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

        let _parent = attrs
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

#[derive(Debug)]
struct FakeProvider;

impl Provider for FakeProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut simulacra_types::ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async {
            Ok(ProviderResponse {
                message: Message {
                    role: Role::Assistant,
                    content: "ok".into(),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
                token_usage: TokenUsage::default(),
                finish_reason: FinishReason::EndTurn,
                provider_response_id: Some("resp-s020".into()),
                model: "claude-sonnet-4-20250514".into(),
            })
        })
    }
}

fn capture_trace<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    install_capture_layer();
    let spans = CAPTURED_SPANS
        .get()
        .expect("captured spans store should be installed");
    let events = CAPTURED_EVENTS
        .get()
        .expect("captured events store should be installed");
    spans.lock().unwrap().clear();
    events.lock().unwrap().clear();
    let result = f();
    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

fn install_capture_layer() {
    CAPTURE_INSTALL.get_or_init(|| {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        CAPTURED_SPANS
            .set(Arc::clone(&spans))
            .expect("span capture store should only initialize once");
        CAPTURED_EVENTS
            .set(Arc::clone(&events))
            .expect("event capture store should only initialize once");

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

fn field_matches(fields: &HashMap<String, String>, key: &str, expected: &str) -> bool {
    fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

fn span_has_field(spans: &[CapturedSpan], name: &str, key: &str, expected: &str) -> bool {
    spans
        .iter()
        .any(|span| span.name == name && field_matches(&span.fields, key, expected))
}

fn event_has_field(events: &[CapturedEvent], level: &str, key: &str, expected: &str) -> bool {
    events
        .iter()
        .any(|event| event.level == level && field_matches(&event.fields, key, expected))
}

fn unique_path(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "simulacra-cli-s020-{label}-{stamp}-{}",
        std::process::id()
    ))
}

struct TempProject {
    root: PathBuf,
}

impl TempProject {
    fn new(label: &str) -> Self {
        let root = unique_path(label);
        fs::create_dir_all(&root).expect("temp project root should be created");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn create_dir(&self, rel: &str) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(&path).expect("fixture directory should be created");
        path
    }

    fn write(&self, rel: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent directories should be created");
        }
        fs::write(&path, contents).expect("fixture file should be written");
        path
    }

    fn set_len(&self, rel: &str, bytes: u64) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("sparse-file parent should be created");
        }
        let file = File::create(&path).expect("sparse file should be created");
        file.set_len(bytes)
            .expect("sparse file length should be set");
        path
    }

    fn write_config(&self, rel: &str, contents: &str) -> PathBuf {
        self.write(rel, contents)
    }

    fn create_files(&self, rel_dir: &str, count: usize) -> PathBuf {
        let dir = self.create_dir(rel_dir);
        for index in 0..count {
            let path = dir.join(format!("file-{index:05}.txt"));
            fs::write(&path, format!("payload-{index}"))
                .expect("fixture file for count limit should be written");
        }
        dir
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct CwdGuard {
    previous: PathBuf,
}

impl CwdGuard {
    fn set(path: &Path) -> Self {
        let previous = std::env::current_dir().expect("current_dir should be readable");
        std::env::set_current_dir(path).expect("current_dir should be set for test");
        Self { previous }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
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

fn cli_args(config_path: impl Into<String>, task: Option<&str>) -> CliArgs {
    CliArgs {
        config_path: config_path.into(),
        task: task.map(str::to_owned),
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: false,
    }
}

fn config_toml(entry_system_prompt: Option<&str>, extra_agents: &str, vfs_block: &str) -> String {
    config_toml_with_skills(entry_system_prompt, extra_agents, vfs_block, &[])
}

fn config_toml_with_skills(
    entry_system_prompt: Option<&str>,
    extra_agents: &str,
    vfs_block: &str,
    skills: &[&str],
) -> String {
    let system_prompt_line = entry_system_prompt
        .map(|value| format!("system_prompt = {value:?}\n"))
        .unwrap_or_default();
    let skills_line = if skills.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = skills.iter().map(|s| format!("{s:?}")).collect();
        format!("skills = [{}]\n", items.join(", "))
    };
    format!(
        r#"[project]
name = "simulacra-s020"

[agent_types.default]
model = "claude-sonnet-4-20250514"
{system_prompt_line}{skills_line}max_turns = 7
max_tokens = 4096

[agent_types.default.capabilities]
shell = false
javascript = false
paths_read = ["/workspace/**", "/skills/**", "/prompts/**", "/docs/**", "/context/**", "/mounted/**", "/external/**"]
paths_write = ["/workspace/**"]
{extra_agents}
{vfs_block}
[task]
entry_agent = "default"
task = "mount host fixtures"
"#
    )
}

fn reviewer_agent_block(system_prompt: &str) -> String {
    format!(
        r#"
[agent_types.reviewer]
model = "gpt-5.4"
system_prompt = {system_prompt:?}
max_turns = 3
max_tokens = 1024

[agent_types.reviewer.capabilities]
paths_read = ["/workspace/**", "/prompts/**", "/skills/**"]
"#
    )
}

fn writer_agent_block(system_prompt: &str) -> String {
    format!(
        r#"
[agent_types.writer]
model = "gpt-5.4"
system_prompt = {system_prompt:?}
max_turns = 3
max_tokens = 1024

[agent_types.writer.capabilities]
paths_read = ["/workspace/**", "/prompts/**"]
paths_write = ["/workspace/**"]
"#
    )
}

fn vfs_block(lines: &str) -> String {
    format!("[vfs]\n{lines}\n")
}

fn mount_entry(source: &str, target: &str) -> String {
    format!("[[vfs.mounts]]\nsource = {source:?}\ntarget = {target:?}\n")
}

fn read_utf8(vfs: &Arc<dyn VirtualFs>, path: &str) -> String {
    String::from_utf8(vfs.read(path).expect("vfs file should exist and be utf-8"))
        .expect("vfs file should be utf-8")
}

fn assert_cli_run_has_project_root(spans: &[CapturedSpan], expected_root: &Path) {
    assert!(
        span_has_field(
            spans,
            "cli_run",
            "simulacra.project.root",
            &expected_root.to_string_lossy()
        ),
        "cli_run span should record simulacra.project.root={}",
        expected_root.display()
    );
}

#[cfg(unix)]
fn symlink_path(src: &Path, dst: &Path) {
    std::os::unix::fs::symlink(src, dst).expect("symlink fixture should be created");
}

#[test]
fn absolute_config_path_uses_its_parent_as_project_root_and_records_it_in_cli_run() {
    let _guard = test_guard();
    let project = TempProject::new("project-root-absolute");
    let config_dir = project.create_dir("config-dir");
    let config_path = project.write_config("config-dir/simulacra.toml", &config_toml(None, "", ""));

    let (_output, spans, _events) = capture_trace(|| {
        run_with_provider(
            cli_args(
                config_path.to_string_lossy().into_owned(),
                Some("task from cli"),
            ),
            Box::new(FakeProvider),
        )
        .expect("cli run should succeed")
    });

    assert_cli_run_has_project_root(&spans, &config_dir);
}

#[test]
fn relative_config_paths_are_resolved_to_absolute_before_project_root_is_computed() {
    let _guard = test_guard();
    let workspace = TempProject::new("project-root-relative-cwd");
    let project = workspace.create_dir("nested/project");
    let config_path =
        workspace.write_config("nested/project/simulacra.toml", &config_toml(None, "", ""));
    let _cwd = CwdGuard::set(workspace.path());
    let relative = config_path
        .strip_prefix(workspace.path())
        .expect("config path should be under cwd")
        .to_string_lossy()
        .into_owned();

    let (_output, spans, _events) = capture_trace(|| {
        run_with_provider(
            cli_args(relative, Some("task from relative config")),
            Box::new(FakeProvider),
        )
        .expect("cli run should succeed with relative config path")
    });

    assert_cli_run_has_project_root(&spans, &project);
}

// Regression guard: ad-hoc mode should never auto-mount skills or prompts,
// even when S020 mounting is implemented for projects with simulacra.toml.
// This test may pass before S020 implementation (since no mounting exists yet).
#[test]
fn adhoc_mode_uses_the_current_working_directory_as_project_root_and_only_mounts_task_md() {
    let _guard = test_guard();
    let workspace = TempProject::new("adhoc-root");
    // Create skills/ in the workspace to ensure ad-hoc mode does NOT auto-mount them
    workspace.write(
        "skills/rust-dev/SKILL.md",
        "---\nname: rust-dev\ndescription: desc\n---\n\nbody",
    );
    let missing_config = workspace.path().join("missing.toml");
    let _cwd = CwdGuard::set(workspace.path());

    let args = cli_args(
        missing_config.to_string_lossy().into_owned(),
        Some("adhoc task body"),
    );
    let boot = bootstrap(&args).expect("adhoc bootstrap should succeed");

    assert_eq!(
        read_utf8(&boot.vfs, "/workspace/task.md"),
        "adhoc task body"
    );
    assert!(
        !boot.vfs.exists("/skills") && !boot.vfs.exists("/skills/rust-dev/SKILL.md"),
        "adhoc mode should not auto-mount skills even when the directory exists in CWD"
    );
    assert!(
        !boot.vfs.exists("/prompts"),
        "adhoc mode should not auto-mount prompts"
    );
}

#[test]
fn adhoc_mode_records_the_current_working_directory_as_project_root_in_cli_run_span() {
    let _guard = test_guard();
    let workspace = TempProject::new("adhoc-root-span");
    let missing_config = workspace.path().join("missing.toml");
    let _cwd = CwdGuard::set(workspace.path());

    let (_output, spans, _events) = capture_trace(|| {
        let args = cli_args(
            missing_config.to_string_lossy().into_owned(),
            Some("adhoc task body"),
        );
        run_with_provider(args, Box::new(FakeProvider))
            .expect("adhoc run should synthesize a default config")
    });

    assert_cli_run_has_project_root(&spans, workspace.path());
}

#[test]
fn relative_mount_sources_resolve_against_the_project_root() {
    let _guard = test_guard();
    let project = TempProject::new("relative-source");
    project.write("config/prompts/planner.md", "plan from project root");
    let config_path = project.write_config(
        "config/simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("prompts", "/prompts"))),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap with relative mount source should succeed");

    assert_eq!(
        read_utf8(&boot.vfs, "/prompts/planner.md"),
        "plan from project root"
    );
}

#[test]
fn absolute_mount_sources_use_the_host_path_directly() {
    let _guard = test_guard();
    let project = TempProject::new("absolute-source-config");
    let external = TempProject::new("absolute-source-host");
    external.write("external/readme.md", "absolute mount fixture");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&mount_entry(
                &external.path().join("external").to_string_lossy(),
                "/external",
            )),
        ),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should accept absolute source mounts");

    assert_eq!(
        read_utf8(&boot.vfs, "/external/readme.md"),
        "absolute mount fixture"
    );
}

#[cfg(unix)]
#[test]
fn tilde_prefixed_mount_sources_expand_to_the_users_home_directory() {
    let _guard = test_guard();
    let home = TempProject::new("tilde-home");
    let project = TempProject::new("tilde-project");
    home.write(
        "simulacra-skills/custom/SKILL.md",
        "---\nname: custom\ndescription: desc\n---\n\nbody",
    );
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&mount_entry("~/simulacra-skills", "/skills/external")),
        ),
    );
    let _home = EnvGuard::set("HOME", Some(&home.path().to_string_lossy()));

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should expand ~ in mount sources on unix");

    assert!(
        boot.vfs.exists("/skills/external/custom/SKILL.md"),
        "tilde-expanded mount should appear in the VFS"
    );
}

#[test]
fn environment_variables_in_mount_source_are_not_expanded() {
    let _guard = test_guard();
    let project = TempProject::new("no-env-expansion");
    // Create a directory whose name literally contains "$HOME" to ensure env vars are not expanded.
    // The source path uses $HOME which should NOT be expanded per spec behavior 7.
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&mount_entry("$HOME/stuff", "/context")),
        ),
    );

    // Because $HOME/stuff is treated as a literal relative path (not expanded),
    // it resolves to <project_root>/$HOME/stuff which does not exist.
    // This should fail with a missing-source error, proving env vars are not expanded.
    let result = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ));
    let error = match result {
        Ok(_) => panic!(
            "bootstrap should fail because $HOME is not expanded and the literal path does not exist"
        ),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(
        message.contains("$HOME") || message.contains("stuff"),
        "error should reference the literal unexpanded path, got: {message}"
    );
}

#[test]
fn mount_targets_without_a_leading_slash_are_startup_errors_that_name_the_invalid_target() {
    let _guard = test_guard();
    let project = TempProject::new("target-without-slash");
    project.write("prompts/planner.md", "planner");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("prompts", "prompts"))),
    );

    let result = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ));
    let error = match result {
        Ok(_) => panic!("bootstrap should fail when a mount target is not absolute"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(
        message.contains("prompts") && message.contains("absolute path"),
        "startup error should name the invalid target, got: {message}"
    );
}

#[test]
fn missing_mount_sources_fail_startup_and_name_the_missing_host_path() {
    let _guard = test_guard();
    let project = TempProject::new("missing-source");
    let missing = project.path().join("does-not-exist");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&mount_entry("does-not-exist", "/context")),
        ),
    );

    let result = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ));
    let error = match result {
        Ok(_) => panic!("bootstrap should fail when a mount source is missing"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(
        message.contains(missing.to_string_lossy().as_ref()) || message.contains("does-not-exist"),
        "startup error should name the missing source path, got: {message}"
    );
}

#[test]
fn mounting_to_the_vfs_root_is_a_startup_error() {
    let _guard = test_guard();
    let project = TempProject::new("target-root");
    project.write("prompts/planner.md", "planner");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("prompts", "/"))),
    );

    let result = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ));
    let error = match result {
        Ok(_) => panic!("mounting to / should fail startup"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(
        message.contains("/") && message.contains("root"),
        "startup error should mention that mounting to / is invalid, got: {message}"
    );
}

#[test]
fn overlapping_directory_mounts_union_their_files() {
    let _guard = test_guard();
    let project = TempProject::new("union-merge");
    project.write("left/a.md", "left file");
    project.write("right/b.md", "right file");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&format!(
                "{}{}",
                mount_entry("left", "/skills"),
                mount_entry("right", "/skills")
            )),
        ),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("union-merge bootstrap should succeed");

    assert!(
        boot.vfs.exists("/skills/a.md"),
        "left-side file should survive the merge"
    );
    assert!(
        boot.vfs.exists("/skills/b.md"),
        "right-side file should survive the merge"
    );
}

#[test]
fn overlapping_file_mounts_use_last_writer_wins_semantics() {
    let _guard = test_guard();
    let project = TempProject::new("file-overwrite");
    project.write("first/shared.md", "first");
    project.write("second/shared.md", "second");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&format!(
                "{}{}",
                mount_entry("first", "/skills"),
                mount_entry("second", "/skills")
            )),
        ),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should accept overlapping file mounts");

    assert_eq!(read_utf8(&boot.vfs, "/skills/shared.md"), "second");
}

#[test]
fn empty_mount_arrays_are_valid_and_leave_only_automatic_mounts() {
    let _guard = test_guard();
    let project = TempProject::new("empty-mounts");
    project.write(
        "skills/rust-dev/SKILL.md",
        "---\nname: rust-dev\ndescription: desc\n---\n\nbody",
    );
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block("auto_mount_skills = true")),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should allow [vfs] with no explicit mounts");

    assert!(
        boot.vfs.exists("/skills/rust-dev/SKILL.md"),
        "automatic skill mount should still apply when [[vfs.mounts]] is empty"
    );
}

#[test]
fn simulacra_config_serialization_exposes_vfs_defaults_when_the_section_is_absent() {
    let _guard = test_guard();
    let project = TempProject::new("vfs-defaults-json");
    let config_path = project.write_config("simulacra.toml", &config_toml(None, "", ""));

    let config = SimulacraConfig::from_file(&config_path.to_string_lossy())
        .expect("config without [vfs] should deserialize");
    let value = serde_json::to_value(&config).expect("config should serialize to json");

    assert_eq!(
        value.pointer("/vfs/auto_mount_skills"),
        Some(&Value::Bool(true)),
        "missing [vfs] should deserialize with auto_mount_skills=true"
    );
    assert_eq!(
        value.pointer("/vfs/max_files_per_mount"),
        Some(&Value::from(10_000u64)),
        "missing [vfs] should deserialize with the default file limit"
    );
    assert_eq!(
        value.pointer("/vfs/max_bytes_per_mount"),
        Some(&Value::from(104_857_600u64)),
        "missing [vfs] should deserialize with the default byte limit"
    );
    assert_eq!(
        value.pointer("/vfs/mounts"),
        Some(&Value::Array(vec![])),
        "missing [vfs] should deserialize with an empty mounts array"
    );
}

#[test]
fn simulacra_config_serialization_round_trips_vfs_mount_entries() {
    let _guard = test_guard();
    let project = TempProject::new("vfs-mount-roundtrip");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&format!(
                "auto_mount_skills = false\nmax_files_per_mount = 2\nmax_bytes_per_mount = 9\n{}",
                mount_entry("prompts", "/prompts")
            )),
        ),
    );

    let config = SimulacraConfig::from_file(&config_path.to_string_lossy())
        .expect("config with [vfs] should deserialize");
    let value = serde_json::to_value(&config).expect("config should serialize to json");

    assert_eq!(
        value.pointer("/vfs/mounts/0/source"),
        Some(&Value::String("prompts".into())),
        "mount source should round-trip through SimulacraConfig"
    );
    assert_eq!(
        value.pointer("/vfs/mounts/0/target"),
        Some(&Value::String("/prompts".into())),
        "mount target should round-trip through SimulacraConfig"
    );
}

#[test]
fn project_skills_directory_is_auto_mounted_into_the_vfs() {
    let _guard = test_guard();
    let project = TempProject::new("auto-skills");
    project.write(
        "skills/rust-dev/SKILL.md",
        "---\nname: rust-dev\ndescription: desc\n---\n\nbody",
    );
    let config_path = project.write_config("simulacra.toml", &config_toml(None, "", ""));

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should auto-mount project skills");

    assert!(boot.vfs.exists("/skills/rust-dev/SKILL.md"));
}

#[test]
fn auto_mount_skills_false_suppresses_the_automatic_skill_mount() {
    let _guard = test_guard();
    let project = TempProject::new("auto-skills-disabled");
    project.write(
        "skills/rust-dev/SKILL.md",
        "---\nname: rust-dev\ndescription: desc\n---\n\nbody",
    );
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block("auto_mount_skills = false")),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should respect auto_mount_skills=false");
    let config_json = serde_json::to_value(&boot.config).expect("config should serialize");

    assert!(
        !boot.vfs.exists("/skills/rust-dev/SKILL.md"),
        "auto_mount_skills=false should suppress the automatic skill mount"
    );
    assert_eq!(
        config_json.pointer("/vfs/auto_mount_skills"),
        Some(&Value::Bool(false)),
        "boot config should preserve the explicit auto_mount_skills=false override"
    );
}

#[test]
fn relative_system_prompt_paths_are_mounted_while_absolute_and_inline_prompts_are_not() {
    let _guard = test_guard();
    let project = TempProject::new("system-prompt-mounting");
    project.write("prompts/planner.md", "planner prompt from host");
    let absolute_prompt = project.write("absolute/prompt.md", "absolute prompt body");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            Some("prompts/planner.md"),
            &(reviewer_agent_block(&absolute_prompt.to_string_lossy())
                + &writer_agent_block("You are an inline helper.")),
            "",
        ),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should mount relative system prompt paths");

    assert_eq!(
        read_utf8(&boot.vfs, "/prompts/planner.md"),
        "planner prompt from host"
    );
    assert!(
        !boot.vfs.exists(&absolute_prompt.to_string_lossy()),
        "absolute host prompt paths should not be mirrored into the VFS root"
    );
    assert!(
        !boot.vfs.exists("/You are an inline helper."),
        "inline prompt strings should not be treated as mount sources"
    );
}

#[test]
fn missing_entry_agent_system_prompt_is_a_startup_error() {
    let _guard = test_guard();
    let project = TempProject::new("missing-entry-prompt");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(Some("prompts/missing-entry.md"), "", ""),
    );

    let result = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ));
    let error = match result {
        Ok(_) => panic!("bootstrap should fail when the entry prompt is missing"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(
        message.contains("prompts/missing-entry.md") && message.contains("entry"),
        "startup error should name the missing entry-agent prompt path, got: {message}"
    );
}

#[test]
fn missing_non_entry_system_prompts_emit_warn_events_and_are_skipped() {
    let _guard = test_guard();
    let project = TempProject::new("missing-non-entry-prompt");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            &reviewer_agent_block("prompts/reviewer-missing.md"),
            "",
        ),
    );

    let (boot, _spans, events) = capture_trace(|| {
        bootstrap(&cli_args(
            config_path.to_string_lossy().into_owned(),
            Some("task"),
        ))
        .expect("bootstrap should skip missing non-entry prompts")
    });

    assert!(
        !boot.vfs.exists("/prompts/reviewer-missing.md"),
        "missing non-entry prompts should be skipped rather than mounted"
    );
    assert!(
        event_has_field(
            &events,
            "WARN",
            "message",
            "missing non-entry-agent system prompt skipped"
        ) || event_has_field(
            &events,
            "WARN",
            "simulacra.vfs.mount.source",
            "prompts/reviewer-missing.md"
        ),
        "bootstrap should emit a WARN-level event for missing non-entry prompts"
    );
}

#[test]
fn mount_copies_the_full_host_directory_tree_recursively() {
    let _guard = test_guard();
    let project = TempProject::new("recursive-copy");
    project.write("source/root.md", "root");
    project.write("source/deep/nested/file.txt", "nested");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("source", "/mounted"))),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should recursively copy host directory trees");

    assert!(boot.vfs.exists("/mounted/root.md"));
    assert!(boot.vfs.exists("/mounted/deep/nested/file.txt"));
}

#[cfg(unix)]
#[test]
fn symlinked_mount_entries_are_followed_and_copied_into_the_vfs() {
    let _guard = test_guard();
    let project = TempProject::new("symlink-follow");
    project.write("shared/target.txt", "through symlink");
    project.create_dir("source");
    symlink_path(
        &project.path().join("shared"),
        &project.path().join("source/linkdir"),
    );
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("source", "/mounted"))),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should follow host symlinks during mount copy");

    assert_eq!(
        read_utf8(&boot.vfs, "/mounted/linkdir/target.txt"),
        "through symlink"
    );
}

#[cfg(unix)]
#[test]
fn symlink_loops_are_detected_skipped_and_warned_about() {
    let _guard = test_guard();
    let project = TempProject::new("symlink-loop");
    project.create_dir("source");
    symlink_path(
        &project.path().join("source"),
        &project.path().join("source/loop"),
    );
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("source", "/mounted"))),
    );

    let (boot, _spans, events) = capture_trace(|| {
        bootstrap(&cli_args(
            config_path.to_string_lossy().into_owned(),
            Some("task"),
        ))
        .expect("bootstrap should skip symlink loops with a warning")
    });

    assert!(
        !boot.vfs.exists("/mounted/loop/loop/loop"),
        "symlink loop entries should be skipped rather than copied recursively forever"
    );
    assert!(
        event_has_field(
            &events,
            "WARN",
            "simulacra.vfs.loop_path",
            &project.path().join("source/loop").to_string_lossy()
        ) || events.iter().any(|event| {
            event.level == "WARN"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("loop") || value.contains("symlink"))
        }),
        "symlink loop detection should emit a WARN-level event naming the loop path"
    );
}

#[test]
fn empty_host_directories_are_created_in_the_vfs() {
    let _guard = test_guard();
    let project = TempProject::new("empty-dir");
    project.create_dir("source/empty/nested");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("source", "/mounted"))),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should create empty directories in the VFS");

    let metadata = boot
        .vfs
        .metadata("/mounted/empty/nested")
        .expect("empty mounted directory should exist");
    assert!(
        metadata.is_dir,
        "empty host directories should become VFS directories"
    );
}

#[test]
fn hidden_files_are_included_in_mount_copies() {
    let _guard = test_guard();
    let project = TempProject::new("hidden-files");
    project.write("source/.env", "SECRET=1");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("source", "/mounted"))),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should include hidden files in mount copies");

    assert_eq!(read_utf8(&boot.vfs, "/mounted/.env"), "SECRET=1");
}

#[test]
fn mounts_are_point_in_time_snapshots_that_do_not_track_later_host_changes() {
    let _guard = test_guard();
    let project = TempProject::new("snapshot-copy");
    let host_file = project.write("source/context.md", "before mount");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("source", "/context"))),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should mount a snapshot copy");
    fs::write(&host_file, "after mount").expect("host fixture should mutate after bootstrap");

    assert_eq!(read_utf8(&boot.vfs, "/context/context.md"), "before mount");
}

#[test]
fn large_files_are_copied_as_raw_bytes_without_transformation() {
    let _guard = test_guard();
    let project = TempProject::new("large-bytes");
    let payload = vec![0, 255, 1, 2, 3, 128, 64, 10];
    project.write("source/payload.bin", &payload);
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("source", "/mounted"))),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should copy binary files unchanged");

    assert_eq!(
        boot.vfs
            .read("/mounted/payload.bin")
            .expect("payload should exist"),
        payload,
        "binary files should be copied byte-for-byte"
    );
}

#[test]
fn mount_copying_emits_mount_spans_but_no_journal_append_spans_during_bootstrap() {
    let _guard = test_guard();
    let project = TempProject::new("no-journal-during-mount");
    project.write("source/file.txt", "host payload");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("source", "/mounted"))),
    );

    let (_boot, spans, _events) = capture_trace(|| {
        bootstrap(&cli_args(
            config_path.to_string_lossy().into_owned(),
            Some("task"),
        ))
        .expect("bootstrap should succeed")
    });

    assert!(
        spans.iter().any(|span| field_matches(
            &span.fields,
            "simulacra.operation.name",
            "vfs_mount"
        )),
        "each mount should emit a vfs_mount span during bootstrap"
    );
    assert!(
        !spans.iter().any(|span| field_matches(
            &span.fields,
            "simulacra.operation.name",
            "journal_append"
        )),
        "mount setup should not emit journal_append spans during bootstrap"
    );
}

#[test]
fn exceeding_the_default_file_limit_fails_bootstrap() {
    let _guard = test_guard();
    let project = TempProject::new("default-file-limit");
    project.create_files("source", 10_001);
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("source", "/mounted"))),
    );

    let result = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ));
    let error = match result {
        Ok(_) => panic!("bootstrap should fail when a mount exceeds the default file limit"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(
        message.contains("10") && message.contains("file") && message.contains("/mounted"),
        "file-limit error should name the mount and file count, got: {message}"
    );
}

#[test]
fn exceeding_the_default_byte_limit_fails_bootstrap() {
    let _guard = test_guard();
    let project = TempProject::new("default-byte-limit");
    project.set_len("source/huge.bin", 104_857_601);
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("source", "/mounted"))),
    );

    let result = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ));
    let error = match result {
        Ok(_) => panic!("bootstrap should fail when a mount exceeds the default byte limit"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(
        message.contains("byte") && message.contains("104857600") && message.contains("/mounted"),
        "byte-limit error should name the mount and configured limit, got: {message}"
    );
}

#[test]
fn approaching_eighty_percent_of_the_file_limit_emits_a_warn_event() {
    let _guard = test_guard();
    let project = TempProject::new("approaching-file-limit");
    project.create_files("source", 4);
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&format!(
                "max_files_per_mount = 5\n{}",
                mount_entry("source", "/mounted")
            )),
        ),
    );

    let (_boot, _spans, events) = capture_trace(|| {
        bootstrap(&cli_args(
            config_path.to_string_lossy().into_owned(),
            Some("task"),
        ))
        .expect("bootstrap should succeed near the file limit")
    });

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("4/5") || value.contains("approaching file limit"))
        }),
        "approaching 80% of max_files_per_mount should emit a WARN event"
    );
}

#[test]
fn approaching_eighty_percent_of_the_byte_limit_emits_a_warn_event() {
    let _guard = test_guard();
    let project = TempProject::new("approaching-byte-limit");
    project.write("source/payload.bin", vec![1u8; 8]);
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&format!(
                "max_bytes_per_mount = 10\n{}",
                mount_entry("source", "/mounted")
            )),
        ),
    );

    let (_boot, _spans, events) = capture_trace(|| {
        bootstrap(&cli_args(
            config_path.to_string_lossy().into_owned(),
            Some("task"),
        ))
        .expect("bootstrap should succeed near the byte limit")
    });

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("8/10") || value.contains("approaching size limit"))
        }),
        "approaching 80% of max_bytes_per_mount should emit a WARN event"
    );
}

#[test]
fn mount_limits_apply_per_mount_not_globally() {
    let _guard = test_guard();
    let project = TempProject::new("per-mount-limits");
    project.create_files("source-a", 4);
    project.create_files("source-b", 4);
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&format!(
                "max_files_per_mount = 5\n{}{}",
                mount_entry("source-a", "/mounted/a"),
                mount_entry("source-b", "/mounted/b")
            )),
        ),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("each mount should get its own limit accounting");

    assert!(boot.vfs.exists("/mounted/a/file-00000.txt"));
    assert!(boot.vfs.exists("/mounted/b/file-00000.txt"));
}

#[test]
fn custom_vfs_limits_override_the_defaults() {
    let _guard = test_guard();
    let project = TempProject::new("custom-limits");
    project.create_files("source", 2);
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&format!(
                "max_files_per_mount = 1\n{}",
                mount_entry("source", "/mounted")
            )),
        ),
    );

    let result = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ));
    let error = match result {
        Ok(_) => panic!("custom max_files_per_mount should override the defaults"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(
        message.contains("1") && message.contains("2") && message.contains("/mounted"),
        "custom file-limit error should mention the override and actual count, got: {message}"
    );
}

#[test]
fn automatic_skill_mounts_run_before_configured_mounts() {
    let _guard = test_guard();
    let project = TempProject::new("ordering-auto-before-config");
    project.write(
        "skills/rust-dev/SKILL.md",
        "---\nname: rust-dev\ndescription: desc\n---\n\nfrom skills",
    );
    project.write(
        "overlay/rust-dev/SKILL.md",
        "---\nname: rust-dev\ndescription: desc\n---\n\nfrom config",
    );
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("overlay", "/skills"))),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should mount skills before configured overlays");

    assert_eq!(
        read_utf8(&boot.vfs, "/skills/rust-dev/SKILL.md"),
        "---\nname: rust-dev\ndescription: desc\n---\n\nfrom config"
    );
}

#[test]
fn configured_mounts_run_in_declaration_order() {
    let _guard = test_guard();
    let project = TempProject::new("ordering-config-declaration");
    project.write("first/shared.txt", "first");
    project.write("second/shared.txt", "second");
    project.write("third/shared.txt", "third");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&format!(
                "{}{}{}",
                mount_entry("first", "/mounted"),
                mount_entry("second", "/mounted"),
                mount_entry("third", "/mounted")
            )),
        ),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should process configured mounts in declaration order");

    assert_eq!(read_utf8(&boot.vfs, "/mounted/shared.txt"), "third");
}

#[test]
fn workspace_task_md_is_seeded_after_mounts_finish() {
    let _guard = test_guard();
    let project = TempProject::new("ordering-task-md");
    project.write("workspace/task.md", "from mount");
    project.write("workspace/notes.md", "mounted note");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&mount_entry("workspace", "/workspace")),
        ),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task wins"),
    ))
    .expect("bootstrap should mount into /workspace before seeding task.md");

    assert_eq!(read_utf8(&boot.vfs, "/workspace/task.md"), "task wins");
    assert!(
        boot.vfs.exists("/workspace/notes.md"),
        "other mounted /workspace files should survive task.md pre-seeding"
    );
}

#[test]
fn mounted_paths_exist_in_the_vfs_but_do_not_bypass_paths_read_restrictions() {
    let _guard = test_guard();
    let project = TempProject::new("capability-read");
    project.write("prompts/planner.md", "planner prompt");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(Some("prompts/planner.md"), "", ""),
    );
    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should mount the entry prompt");

    assert!(
        boot.vfs.exists("/prompts/planner.md"),
        "mounted prompt should exist in the VFS before capability checks run"
    );

    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = AgentCell::new(
        Arc::clone(&boot.vfs),
        CapabilityToken {
            paths_read: vec![PathPattern("/workspace/**".into())],
            paths_write: vec![PathPattern("/workspace/**".into())],
            ..Default::default()
        },
        Arc::new(Mutex::new(ResourceBudget::new(8_192, 7, Decimal::ZERO, 0))),
        journal,
        http_client,
    );

    let error = cell
        .read_file("/prompts/planner.md")
        .expect_err("capability token should deny reads outside /workspace/**");
    assert!(
        error.to_string().contains("read access denied")
            && error.to_string().contains("/prompts/planner.md"),
        "capability denial should surface the denied mounted path, got: {error}"
    );
}

#[test]
fn mounted_paths_do_not_bypass_paths_write_restrictions() {
    let _guard = test_guard();
    let project = TempProject::new("capability-write");
    project.write("prompts/planner.md", "planner prompt");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(Some("prompts/planner.md"), "", ""),
    );
    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should mount the entry prompt");

    assert!(boot.vfs.exists("/prompts/planner.md"));

    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = AgentCell::new(
        Arc::clone(&boot.vfs),
        CapabilityToken {
            paths_read: vec![PathPattern("/prompts/**".into())],
            paths_write: vec![PathPattern("/workspace/**".into())],
            ..Default::default()
        },
        Arc::new(Mutex::new(ResourceBudget::new(8_192, 7, Decimal::ZERO, 0))),
        journal,
        http_client,
    );

    let error = cell
        .write_file("/prompts/planner.md", b"mutated")
        .expect_err("capability token should deny writes outside /workspace/**");
    assert!(
        error.to_string().contains("write access denied")
            && error.to_string().contains("/prompts/planner.md"),
        "write denial should identify the mounted path, got: {error}"
    );
}

#[test]
fn mounted_paths_outside_paths_read_remain_inaccessible_even_though_the_vfs_contains_them() {
    let _guard = test_guard();
    let project = TempProject::new("capability-list-dir");
    project.write(
        "skills/review/SKILL.md",
        "---\nname: review\ndescription: desc\n---\n\nbody",
    );
    let config_path = project.write_config("simulacra.toml", &config_toml(None, "", ""));
    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should auto-mount skills");

    assert!(boot.vfs.exists("/skills/review/SKILL.md"));

    let journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = AgentCell::new(
        Arc::clone(&boot.vfs),
        CapabilityToken {
            paths_read: vec![PathPattern("/workspace/**".into())],
            ..Default::default()
        },
        Arc::new(Mutex::new(ResourceBudget::new(8_192, 7, Decimal::ZERO, 0))),
        journal,
        http_client,
    );

    let error = cell
        .list_dir("/skills")
        .expect_err("list_dir should also honor paths_read restrictions");
    assert!(
        error.to_string().contains("/skills"),
        "list_dir denial should identify the inaccessible mounted path, got: {error}"
    );
}

#[test]
fn each_mount_entry_emits_a_vfs_mount_span() {
    let _guard = test_guard();
    let project = TempProject::new("observability-span-per-mount");
    project.write("first/a.txt", "a");
    project.write("second/b.txt", "b");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&format!(
                "{}{}",
                mount_entry("first", "/first"),
                mount_entry("second", "/second")
            )),
        ),
    );

    let (_boot, spans, _events) = capture_trace(|| {
        bootstrap(&cli_args(
            config_path.to_string_lossy().into_owned(),
            Some("task"),
        ))
        .expect("bootstrap should succeed")
    });

    let vfs_mount_spans = spans
        .iter()
        .filter(|span| field_matches(&span.fields, "simulacra.operation.name", "vfs_mount"))
        .count();
    assert_eq!(
        vfs_mount_spans, 2,
        "expected one vfs_mount span per configured mount"
    );
}

#[test]
fn mount_spans_include_source_target_and_file_count_fields() {
    let _guard = test_guard();
    let project = TempProject::new("observability-span-fields");
    project.write("source/file.txt", "payload");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(None, "", &vfs_block(&mount_entry("source", "/mounted"))),
    );

    let (_boot, spans, _events) = capture_trace(|| {
        bootstrap(&cli_args(
            config_path.to_string_lossy().into_owned(),
            Some("task"),
        ))
        .expect("bootstrap should succeed")
    });

    let mount_span = spans
        .iter()
        .find(|span| field_matches(&span.fields, "simulacra.operation.name", "vfs_mount"))
        .expect("expected a vfs_mount span");
    assert!(
        mount_span.fields.contains_key("simulacra.vfs.mount.source")
            && mount_span.fields.contains_key("simulacra.vfs.mount.target")
            && mount_span
                .fields
                .contains_key("simulacra.vfs.mount.file_count"),
        "vfs_mount spans should carry source, target, and file_count fields"
    );
}

#[test]
fn automatic_and_configured_mounts_record_their_origin_in_mount_spans() {
    let _guard = test_guard();
    let project = TempProject::new("observability-origin");
    project.write(
        "skills/rust-dev/SKILL.md",
        "---\nname: rust-dev\ndescription: desc\n---\n\nbody",
    );
    project.write("prompts/planner.md", "planner prompt");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            Some("prompts/planner.md"),
            "",
            &vfs_block(&mount_entry("prompts", "/prompts-override")),
        ),
    );

    let (_boot, spans, _events) = capture_trace(|| {
        bootstrap(&cli_args(
            config_path.to_string_lossy().into_owned(),
            Some("task"),
        ))
        .expect("bootstrap should succeed")
    });

    assert!(
        spans
            .iter()
            .any(|span| field_matches(&span.fields, "simulacra.vfs.mount.origin", "auto")),
        "automatic mounts should be tagged with simulacra.vfs.mount.origin=auto"
    );
    assert!(
        spans.iter().any(|span| field_matches(
            &span.fields,
            "simulacra.vfs.mount.origin",
            "config"
        )),
        "configured mounts should be tagged with simulacra.vfs.mount.origin=config"
    );
}

#[test]
fn mount_failures_emit_error_events_with_the_source_path_and_reason() {
    let _guard = test_guard();
    let project = TempProject::new("observability-error-event");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&mount_entry("missing-source", "/mounted")),
        ),
    );

    let (_result, _spans, events) = capture_trace(|| {
        bootstrap(&cli_args(
            config_path.to_string_lossy().into_owned(),
            Some("task"),
        ))
    });

    assert!(
        events.iter().any(|event| {
            event.level == "ERROR"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("missing-source") || value.contains("/mounted"))
        }),
        "mount failures should emit ERROR events that name the source path and failure reason"
    );
}

#[test]
fn mount_size_warnings_emit_warn_events_with_current_usage_and_limits() {
    let _guard = test_guard();
    let project = TempProject::new("observability-limit-warning");
    project.create_files("source", 4);
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(
            None,
            "",
            &vfs_block(&format!(
                "max_files_per_mount = 5\n{}",
                mount_entry("source", "/mounted")
            )),
        ),
    );

    let (_boot, _spans, events) = capture_trace(|| {
        bootstrap(&cli_args(
            config_path.to_string_lossy().into_owned(),
            Some("task"),
        ))
        .expect("bootstrap should succeed near the file threshold")
    });

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("4") && value.contains("5"))
        }),
        "limit warnings should include current usage and configured limits"
    );
}

#[test]
fn bootstrap_completion_emits_an_info_event_with_total_mount_counts_and_files() {
    let _guard = test_guard();
    let project = TempProject::new("observability-bootstrap-complete");
    project.write(
        "skills/rust-dev/SKILL.md",
        "---\nname: rust-dev\ndescription: desc\n---\n\nbody",
    );
    project.write("prompts/planner.md", "planner prompt");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml(Some("prompts/planner.md"), "", ""),
    );

    let (_boot, _spans, events) = capture_trace(|| {
        bootstrap(&cli_args(
            config_path.to_string_lossy().into_owned(),
            Some("task"),
        ))
        .expect("bootstrap should succeed")
    });

    assert!(
        events.iter().any(|event| {
            event.level == "INFO"
                && event.fields.keys().any(|key| {
                    key == "simulacra.vfs.mount.count" || key == "simulacra.vfs.mount.file_total"
                })
        }),
        "bootstrap completion should emit an INFO event with total mount count and total files mounted"
    );
}

// ── Integration: mount → skill discovery ────────────────────────────────
// Verifies that auto-mounting skills/ into the VFS makes them visible to
// discover_and_filter_skills, so the skill catalog is populated.

#[test]
fn auto_mounted_skills_are_discovered_and_appear_in_the_skill_catalog() {
    let _guard = test_guard();
    let project = TempProject::new("mount-to-discovery");
    project.write(
        "skills/rust-dev/SKILL.md",
        "---\nname: rust-dev\ndescription: Rust development helper\n---\n\nRust skill body",
    );
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml_with_skills(None, "", "", &["rust-dev"]),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should auto-mount skills and discover them");

    assert!(
        boot.vfs.exists("/skills/rust-dev/SKILL.md"),
        "skill should be mounted in VFS"
    );
    assert_eq!(
        boot.skill_catalog.len(),
        1,
        "exactly one skill should be discovered from the auto-mounted skills directory"
    );
    assert_eq!(
        boot.skill_catalog[0].name, "rust-dev",
        "discovered skill name should match the SKILL.md frontmatter"
    );
}

// ── Integration: configured mount → skill discovery ─────────────────────
// Verifies that a configured mount placing skill dirs at /skills/ also
// feeds skill discovery correctly.

#[test]
fn configured_mount_to_skills_path_feeds_skill_discovery() {
    let _guard = test_guard();
    let project = TempProject::new("config-mount-discovery");
    project.write(
        "external-skills/code-review/SKILL.md",
        "---\nname: code-review\ndescription: Code review skill\n---\n\nReview body",
    );
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml_with_skills(
            None,
            "",
            &vfs_block(&format!(
                "auto_mount_skills = false\n{}",
                mount_entry("external-skills", "/skills")
            )),
            &["code-review"],
        ),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("task"),
    ))
    .expect("bootstrap should discover skills from configured mount at /skills/");

    assert!(
        boot.vfs.exists("/skills/code-review/SKILL.md"),
        "configured-mount skill should be in VFS"
    );
    assert_eq!(boot.skill_catalog.len(), 1);
    assert_eq!(boot.skill_catalog[0].name, "code-review");
}

// ── Integration: full config parse → bootstrap → VFS mount round-trip ───
// Exercises the complete simulacra.toml → SimulacraConfig → bootstrap → VFS flow
// with a realistic multi-mount configuration.

#[test]
fn full_config_with_multiple_mounts_populates_vfs_correctly() {
    let _guard = test_guard();
    let project = TempProject::new("full-config-roundtrip");
    project.write(
        "skills/rust-dev/SKILL.md",
        "---\nname: rust-dev\ndescription: desc\n---\n\nbody",
    );
    project.write("prompts/planner.md", "planner prompt");
    project.write("docs/design.md", "design doc");
    let config_path = project.write_config(
        "simulacra.toml",
        &config_toml_with_skills(
            Some("prompts/planner.md"),
            "",
            &vfs_block(&format!(
                "{}{}",
                mount_entry("prompts", "/prompts"),
                mount_entry("docs", "/docs"),
            )),
            &["rust-dev"],
        ),
    );

    let boot = bootstrap(&cli_args(
        config_path.to_string_lossy().into_owned(),
        Some("test task"),
    ))
    .expect("full config round-trip bootstrap should succeed");

    // Auto-mounted skills
    assert!(boot.vfs.exists("/skills/rust-dev/SKILL.md"));
    assert_eq!(boot.skill_catalog.len(), 1);
    // Auto-mounted system prompt
    assert_eq!(
        read_utf8(&boot.vfs, "/prompts/planner.md"),
        "planner prompt"
    );
    // Configured mounts
    assert_eq!(read_utf8(&boot.vfs, "/docs/design.md"), "design doc");
    // Task pre-seeded last
    assert_eq!(read_utf8(&boot.vfs, "/workspace/task.md"), "test task");
}
