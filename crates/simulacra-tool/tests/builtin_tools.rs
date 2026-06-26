#![cfg(feature = "sandbox")]

use rust_decimal::Decimal;
use serde_json::{Value, json};
use simulacra_sandbox::AgentCell;
use simulacra_tool::{ToolError, ToolRegistry, register_builtins};
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalError, JournalStorage, PathPattern, ResourceBudget, TokenUsage,
    VirtualFs,
};
use simulacra_vfs::MemoryFs;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Default)]
struct FakeJournalStorage {
    entries: Mutex<Vec<JournalEntry>>,
}

impl JournalStorage for FakeJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        self.entries.lock().unwrap().push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.agent_id == *agent_id)
            .cloned()
            .collect())
    }

    fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        _after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        let snapshot_data =
            serde_json::to_vec(&data).map_err(|error| JournalError::Storage(error.to_string()))?;
        self.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::Checkpoint { snapshot_data },
        })
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if checkpoint_idx >= entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(checkpoint_idx));
        }
        Ok(entries[..=checkpoint_idx].to_vec())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if start_index > entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(start_index));
        }
        Ok(entries[start_index..].to_vec())
    }
}

struct Harness {
    registry: ToolRegistry,
    vfs: Arc<MemoryFs>,
}

impl Harness {
    fn new(capability: CapabilityToken, budget: ResourceBudget) -> Self {
        let vfs = Arc::new(MemoryFs::new());
        let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
        let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        let cell = Arc::new(AgentCell::new(
            vfs_dyn,
            capability,
            Arc::new(Mutex::new(budget)),
            journal,
            http_client,
        ));
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry, Arc::clone(&cell));

        let _ = cell;

        Self { registry, vfs }
    }
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
    current_span: Option<String>,
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
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);

        let parent_name = attrs
            .parent()
            .and_then(|parent| ctx.span(parent).map(|span| span.name().to_string()))
            .or_else(|| {
                ctx.span(id)
                    .and_then(|span| span.parent().map(|parent| parent.name().to_string()))
            })
            .or_else(|| ctx.lookup_current().map(|span| span.name().to_string()));

        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            parent_name,
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
            current_span: ctx.lookup_current().map(|span| span.name().to_string()),
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

fn run_async<F>(future: F) -> F::Output
where
    F: Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn capture_async<R>(operation: impl FnOnce() -> R) -> (R, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    });
    let result = tracing::subscriber::with_default(subscriber, operation);
    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

fn full_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![PathPattern("/**".into())],
        ..Default::default()
    }
}

fn no_read_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        paths_read: vec![],
        paths_write: vec![PathPattern("/**".into())],
        ..Default::default()
    }
}

fn unlimited_budget() -> ResourceBudget {
    ResourceBudget::new(0, 0, Decimal::ZERO, 0)
}

fn budget_with_turns_exhausted() -> ResourceBudget {
    ResourceBudget {
        used_turns: 1,
        ..ResourceBudget::new(0, 1, Decimal::ZERO, 0)
    }
}

fn budget_with_vfs_bytes_exhausted() -> ResourceBudget {
    ResourceBudget {
        max_vfs_bytes: 1,
        used_vfs_bytes: 1,
        ..ResourceBudget::new(0, 0, Decimal::ZERO, 0)
    }
}

fn call_tool(
    harness: &Harness,
    name: &str,
    arguments: Value,
    capability: &CapabilityToken,
) -> Result<Value, ToolError> {
    run_async(harness.registry.call(name, arguments, capability))
}

fn string_result(value: &Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn assert_error_result_contains(value: &Value, expected_substring: &str) {
    assert_eq!(
        value.get("is_error").and_then(Value::as_bool),
        Some(true),
        "expected an error-shaped tool result, got {value:?}"
    );

    let rendered = value.to_string().to_ascii_lowercase();
    assert!(
        rendered.contains(&expected_substring.to_ascii_lowercase()),
        "expected {value:?} to mention {expected_substring:?}"
    );
}

fn assert_invalid_arguments(result: Result<Value, ToolError>) {
    match result {
        Err(ToolError::InvalidArguments(_)) => {}
        other => panic!("expected invalid arguments error, got {other:?}"),
    }
}

#[test]
fn register_builtins_registers_exactly_six_tools() {
    let harness = Harness::new(full_capability(), unlimited_budget());

    assert_eq!(harness.registry.definitions().len(), 6);
}

#[test]
fn tool_registry_definitions_after_register_builtins_have_correct_names_and_descriptions() {
    let harness = Harness::new(full_capability(), unlimited_budget());
    let definitions = harness.registry.definitions();
    let expected = [
        (
            "file_read",
            "Read the contents of a file at the given path.",
        ),
        (
            "file_write",
            "Write content to a file, creating parent directories as needed.",
        ),
        (
            "file_edit",
            "Apply a search-and-replace edit to an existing file.",
        ),
        (
            "shell_exec",
            "Execute a shell command in the agent's virtual shell and return \
                stdout, stderr, and exit code. \
                Supported builtins: echo, cat, ls, mkdir, cp, mv, rm, head, tail, sed, grep, \
                wc, find, sort, uniq, cut, tr, tee, true, false, cd, pwd, env, which, export, \
                curl, wget. \
                Operators: pipes (|), redirects (>, >>), conditional chains (&&, ||), \
                sequence (;). State that persists across calls: env vars and the working \
                directory (cd /tmp; later calls see /tmp as cwd). Interpreter aliases: \
                node <file.js>, node -e <code>, node - for stdin, python <script.py>, \
                python -c <code>, and python - for stdin run through mediated sandbox \
                runtimes. All paths resolve inside the agent's sandbox VFS — there is no \
                host filesystem access.",
        ),
        (
            "js_exec",
            "Execute JavaScript code in QuickJS and return the string result or stdout. Each call gets a fresh JS global/context: variables, prototypes, and module singletons do not persist between calls. Use ESM `import`, not `require`. Available modules include simulacra:fs/fs, simulacra:console, simulacra:process, simulacra:path, and simulacra:crypto. File, fetch, and module-load host operations are mediated by the sandbox.",
        ),
        ("list_dir", "List the contents of a directory."),
    ];

    for (name, description) in expected {
        assert!(
            definitions
                .iter()
                .any(|definition| definition.name == name && definition.description == description),
            "missing definition for {name} with description {description:?}: {definitions:#?}"
        );
    }
}

#[test]
fn each_tool_definition_has_a_valid_json_schema_as_input_schema() {
    let harness = Harness::new(full_capability(), unlimited_budget());
    let definitions = harness.registry.definitions();
    let expected_required = [
        ("file_read", vec!["path"]),
        ("file_write", vec!["path", "content"]),
        ("file_edit", vec!["path", "old_string", "new_string"]),
        ("shell_exec", vec!["command"]),
        ("js_exec", vec!["code"]),
        ("list_dir", vec!["path"]),
    ];

    for (name, required_fields) in expected_required {
        let definition = definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap_or_else(|| panic!("missing definition for {name}"));
        let schema = &definition.input_schema;

        assert_eq!(schema.get("type"), Some(&json!("object")));
        assert!(
            schema.get("properties").is_some(),
            "missing properties for {name}"
        );
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("missing required array for {name}"))
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(required, required_fields);
    }
}

#[test]
fn file_read_with_a_path_that_exists_returns_the_file_content() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/hello.txt", b"hello from simulacra")
        .unwrap();

    let result = call_tool(
        &harness,
        "file_read",
        json!({ "path": "/workspace/hello.txt" }),
        &capability,
    )
    .expect("file_read should succeed");

    assert_eq!(result, json!("hello from simulacra"));
}

#[test]
fn file_read_with_a_path_that_does_not_exist_returns_error_result_with_not_found_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "file_read",
        json!({ "path": "/workspace/missing.txt" }),
        &capability,
    )
    .expect("file_read should return a user-facing error result");

    assert_error_result_contains(&result, "not found");
}

#[test]
fn file_read_without_path_argument_returns_tool_error_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(&harness, "file_read", json!({}), &capability));
}

#[test]
fn file_write_writes_content_and_returns_confirmation_with_byte_count() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "file_write",
        json!({
            "path": "/workspace/note.txt",
            "content": "abc123"
        }),
        &capability,
    )
    .expect("file_write should succeed");

    assert_eq!(harness.vfs.read("/workspace/note.txt").unwrap(), b"abc123");
    let message = string_result(&result);
    assert!(message.contains("/workspace/note.txt"));
    assert!(message.contains('6'));
}

#[test]
fn file_write_to_a_nested_path_creates_parent_directories() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    call_tool(
        &harness,
        "file_write",
        json!({
            "path": "/workspace/deep/tree/file.txt",
            "content": "nested"
        }),
        &capability,
    )
    .expect("file_write should succeed");

    assert!(harness.vfs.exists("/workspace/deep"));
    assert!(harness.vfs.exists("/workspace/deep/tree"));
    assert_eq!(
        harness.vfs.read("/workspace/deep/tree/file.txt").unwrap(),
        b"nested"
    );
}

#[test]
fn file_write_without_content_argument_returns_tool_error_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(
        &harness,
        "file_write",
        json!({ "path": "/workspace/out.txt" }),
        &capability,
    ));
}

#[test]
fn file_edit_replaces_old_string_with_new_string_in_the_file() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"alpha beta gamma")
        .unwrap();

    let result = call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/edit.txt",
            "old_string": "beta",
            "new_string": "delta"
        }),
        &capability,
    )
    .expect("file_edit should succeed");

    assert_eq!(
        harness.vfs.read("/workspace/edit.txt").unwrap(),
        b"alpha delta gamma"
    );
    assert!(!string_result(&result).is_empty());
}

#[test]
fn file_edit_where_old_string_is_not_found_returns_error_result_with_not_found_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"alpha beta gamma")
        .unwrap();

    let result = call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/edit.txt",
            "old_string": "omega",
            "new_string": "delta"
        }),
        &capability,
    )
    .expect("file_edit should return a user-facing error result");

    assert_error_result_contains(&result, "not found");
}

#[test]
fn file_edit_where_old_string_appears_more_than_once_returns_error_result_with_ambiguous_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"repeat and repeat again")
        .unwrap();

    let result = call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/edit.txt",
            "old_string": "repeat",
            "new_string": "done"
        }),
        &capability,
    )
    .expect("file_edit should return a user-facing error result");

    assert_error_result_contains(&result, "ambiguous");
}

#[test]
fn file_edit_on_a_non_existent_file_returns_tool_result_is_error_true() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "file_edit",
        json!({
            "path": "/workspace/missing.txt",
            "old_string": "old",
            "new_string": "new"
        }),
        &capability,
    )
    .expect("file_edit should return a user-facing error result");

    assert_eq!(result.get("is_error").and_then(Value::as_bool), Some(true));
}

#[test]
fn shell_exec_echo_hello_returns_stdout_stderr_and_exit_code() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "echo hello" }),
        &capability,
    )
    .expect("shell_exec should succeed");

    assert_eq!(
        result,
        json!({
            "stdout": "hello\n",
            "stderr": "",
            "exit_code": 0
        })
    );
}

#[test]
fn shell_exec_nonexistent_command_returns_non_zero_exit_code_not_tool_error() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "shell_exec",
        json!({ "command": "nonexistent_command" }),
        &capability,
    )
    .expect("shell_exec should return a normal result even for failed commands");

    let exit_code = result
        .get("exit_code")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    assert_ne!(exit_code, 0);
}

#[test]
fn shell_exec_without_command_argument_returns_tool_error_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(&harness, "shell_exec", json!({}), &capability));
}

#[test]
fn js_exec_one_plus_one_returns_the_string_result() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(&harness, "js_exec", json!({ "code": "1 + 1" }), &capability)
        .expect("js_exec should succeed");

    assert_eq!(result, json!("2"));
}

#[test]
fn js_exec_with_a_syntax_error_returns_tool_result_is_error_true_with_the_error_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "js_exec",
        json!({ "code": "function {" }),
        &capability,
    )
    .expect("js_exec should return a user-facing error result");

    assert_error_result_contains(&result, "error");
}

#[test]
fn js_exec_without_code_argument_returns_tool_error_invalid_arguments() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    assert_invalid_arguments(call_tool(&harness, "js_exec", json!({}), &capability));
}

#[test]
fn list_dir_root_returns_entries_in_the_root_directory() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/readme.md", b"hi").unwrap();
    harness.vfs.write("/todo.txt", b"todo").unwrap();

    let result = call_tool(&harness, "list_dir", json!({ "path": "/" }), &capability)
        .expect("list_dir should succeed");

    let listing = string_result(&result);
    assert!(listing.contains("workspace/"));
    assert!(listing.contains("todo.txt"));
}

#[test]
fn list_dir_on_a_non_existent_path_returns_tool_result_is_error_true() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "list_dir",
        json!({ "path": "/workspace/missing" }),
        &capability,
    )
    .expect("list_dir should return a user-facing error result");

    assert_eq!(result.get("is_error").and_then(Value::as_bool), Some(true));
}

#[test]
fn directory_entries_are_suffixed_with_slash_in_the_output() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/src/main.rs", b"fn main() {}")
        .unwrap();

    let result = call_tool(
        &harness,
        "list_dir",
        json!({ "path": "/workspace" }),
        &capability,
    )
    .expect("list_dir should succeed");

    let listing = string_result(&result);
    assert!(listing.contains("src/"));
}

#[test]
fn list_dir_directory_suffix_metadata_is_mediated_by_agent_cell_capability() {
    let capability = CapabilityToken {
        paths_read: vec![PathPattern("/workspace".into())],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..Default::default()
    };
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/secret/file.txt", b"classified")
        .unwrap();

    let result = call_tool(
        &harness,
        "list_dir",
        json!({ "path": "/workspace" }),
        &capability,
    );

    match result {
        Err(ToolError::CapabilityDenied(denied)) => {
            assert_eq!(denied.operation, "path_read");
            assert!(
                denied.reason.contains("/workspace/secret"),
                "denial should identify the child path whose metadata was checked, got {:?}",
                denied.reason
            );
        }
        other => panic!("expected mediated metadata capability denial, got {other:?}"),
    }
}

#[test]
fn agent_cell_capability_denial_surfaces_as_tool_error_capability_denied_through_the_tool() {
    let capability = no_read_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/hello.txt", b"hello").unwrap();

    let result = call_tool(
        &harness,
        "file_read",
        json!({ "path": "/workspace/hello.txt" }),
        &capability,
    );

    match result {
        Err(ToolError::CapabilityDenied(_)) => {}
        other => panic!("expected capability denied error, got {other:?}"),
    }
}

#[test]
fn agent_cell_budget_exhaustion_surfaces_as_tool_error_execution_failed_with_resource_details() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), budget_with_vfs_bytes_exhausted());

    let result = call_tool(
        &harness,
        "file_write",
        json!({
            "path": "/workspace/budget.txt",
            "content": "data"
        }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            let lower = message.to_ascii_lowercase();
            assert!(lower.contains("vfs_bytes"));
            assert!(lower.contains("1"));
        }
        other => panic!("expected execution failed error, got {other:?}"),
    }
}

#[test]
fn vfs_errors_from_agent_cell_surface_as_tool_error_execution_failed_with_path_and_error_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let result = call_tool(
        &harness,
        "file_write",
        json!({
            "path": "/",
            "content": "root"
        }),
        &capability,
    );

    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            let lower = message.to_ascii_lowercase();
            assert!(message.contains('/'));
            assert!(lower.contains("not a file"));
        }
        other => panic!("expected execution failed error, got {other:?}"),
    }
}

#[test]
fn each_tool_invocation_produces_a_span_with_gen_ai_tool_name_equal_to_the_tool_name() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/hello.txt", b"hello").unwrap();

    let (_, spans, _) = capture_async(|| {
        run_async(harness.registry.call(
            "file_read",
            json!({ "path": "/workspace/hello.txt" }),
            &capability,
        ))
    });

    assert!(
        spans.iter().any(|span| {
            span.fields
                .get("gen_ai.tool.name")
                .map(|value| value == "file_read")
                .unwrap_or(false)
        }),
        "expected a tool invocation span with gen_ai.tool.name=file_read, got {spans:#?}"
    );
}

#[test]
fn tool_invocation_spans_are_children_of_the_agent_turn_span() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/hello.txt", b"hello").unwrap();

    let (_, spans, _) = capture_async(|| {
        let agent_turn = tracing::info_span!("agent_turn");
        let _guard = agent_turn.enter();
        run_async(harness.registry.call(
            "file_read",
            json!({ "path": "/workspace/hello.txt" }),
            &capability,
        ))
    });

    assert!(
        spans.iter().any(|span| {
            span.fields
                .get("gen_ai.tool.name")
                .map(|value| value == "file_read")
                .unwrap_or(false)
                && span.parent_name.as_deref() == Some("agent_turn")
        }),
        "expected a tool span under agent_turn, got {spans:#?}"
    );
}

#[test]
fn tool_errors_are_logged_at_error_level_with_the_tool_name_and_error_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), budget_with_turns_exhausted());

    let (_, _, events) = capture_async(|| {
        run_async(harness.registry.call(
            "shell_exec",
            json!({ "command": "echo hello" }),
            &capability,
        ))
    });

    assert!(
        events.iter().any(|event| {
            event.level == "ERROR"
                && event
                    .fields
                    .get("gen_ai.tool.name")
                    .map(|value| value == "shell_exec")
                    .unwrap_or(false)
                && event
                    .fields
                    .values()
                    .any(|value| value.to_ascii_lowercase().contains("turns"))
        }),
        "expected an ERROR log with tool name and message, got {events:#?}"
    );
}

#[test]
fn tool_results_are_captured_as_events_on_the_tool_span_per_gen_ai_tool_message_convention() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/hello.txt", b"hello").unwrap();

    let (_, _, events) = capture_async(|| {
        run_async(harness.registry.call(
            "file_read",
            json!({ "path": "/workspace/hello.txt" }),
            &capability,
        ))
    });

    assert!(
        events.iter().any(|event| {
            event.current_span.is_some()
                && event.fields.contains_key("gen_ai.tool.message")
                && event
                    .fields
                    .get("gen_ai.tool.name")
                    .map(|value| value == "file_read")
                    .unwrap_or(false)
        }),
        "expected a gen_ai.tool.message event on the tool span, got {events:#?}"
    );
}

#[test]
fn tool_result_event_message_is_bounded_but_preserves_full_result_length() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    let large_content = "x".repeat(10_000);
    harness
        .vfs
        .write("/workspace/large.txt", large_content.as_bytes())
        .unwrap();

    let (_, _, events) = capture_async(|| {
        run_async(harness.registry.call(
            "file_read",
            json!({ "path": "/workspace/large.txt" }),
            &capability,
        ))
    });

    let event = events
        .iter()
        .find(|event| {
            event
                .fields
                .get("gen_ai.tool.name")
                .map(|value| value == "file_read")
                .unwrap_or(false)
                && event.fields.contains_key("gen_ai.tool.message")
        })
        .expect("expected file_read tool result event");
    let message = event
        .fields
        .get("gen_ai.tool.message")
        .expect("message should be present");

    assert!(
        message.len() < large_content.len(),
        "telemetry message should be bounded, got {} bytes",
        message.len()
    );
    assert_eq!(
        event.fields.get("gen_ai.tool.message_truncated"),
        Some(&"true".to_string())
    );
    assert!(
        event
            .fields
            .get("gen_ai.tool.result_length")
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|len| len > large_content.len()),
        "full serialized result length should be preserved, got {event:?}"
    );
}
