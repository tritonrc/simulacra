use rust_decimal::Decimal;
use simulacra_sandbox::AgentCell;
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalError, JournalStorage, PathPattern, ResourceBudget, TokenUsage,
    VirtualFs,
};
use simulacra_vfs::MemoryFs;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Default)]
struct FakeJournalStorage {
    entries: Mutex<Vec<JournalEntry>>,
}

impl FakeJournalStorage {
    fn entries(&self) -> Vec<JournalEntry> {
        self.entries.lock().unwrap().clone()
    }
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
            .filter(|entry| &entry.agent_id == agent_id)
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

#[derive(Debug, Clone)]
struct CapturedSpan {
    fields: HashMap<String, String>,
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
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
        self.spans.lock().unwrap().push(CapturedSpan { fields });
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
}

fn capture_spans<R>(operation: impl FnOnce() -> R) -> (R, Vec<CapturedSpan>) {
    static GLOBAL_TRACING: OnceLock<()> = OnceLock::new();
    GLOBAL_TRACING.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
        tracing::callsite::rebuild_interest_cache();
    });

    let spans = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
    });
    let result = tracing::subscriber::with_default(subscriber, operation);
    let spans = spans.lock().unwrap().clone();
    (result, spans)
}

fn span_operations(spans: &[CapturedSpan]) -> Vec<String> {
    let mut operations = spans
        .iter()
        .filter_map(|span| span.fields.get("simulacra.operation.name").cloned())
        .collect::<Vec<_>>();
    operations.sort();
    operations
}

struct Harness {
    vfs: Arc<MemoryFs>,
    cell: AgentCell,
}

impl Harness {
    fn new(capability: CapabilityToken, journal: Arc<FakeJournalStorage>) -> Self {
        let vfs = Arc::new(MemoryFs::new());
        let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
        let journal_dyn: Arc<dyn JournalStorage> = journal;
        let budget = Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0)));
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        let cell = AgentCell::new(vfs_dyn, capability, budget, journal_dyn, http_client);
        Self { vfs, cell }
    }
}

fn capability(shell: bool, javascript: bool) -> CapabilityToken {
    capability_with_python(shell, javascript, false)
}

fn capability_with_python(shell: bool, javascript: bool, python: bool) -> CapabilityToken {
    CapabilityToken {
        shell,
        javascript,
        python,
        paths_read: vec![PathPattern("/workspace/**".into())],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..Default::default()
    }
}

#[test]
fn shell_exec_node_script_executes_through_quickjs_and_returns_output() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(capability(true, true), journal);
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write(
        "/workspace/script.js",
        br#"
        console.log("hello from node");
        42;
        "#,
    )
    .expect("seed script");

    let result = harness
        .cell
        .execute_shell("node /workspace/script.js")
        .expect("node alias should execute the script");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stderr, "");
    assert_eq!(result.stdout, "hello from node\n42\n");
}

#[test]
fn shell_exec_node_without_arguments_returns_usage_error_with_exit_code_one() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(capability(true, true), journal);

    let result = harness
        .cell
        .execute_shell("node")
        .expect("node without arguments should return a usage error result");

    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "Usage: node <script.js>\n");
}

#[test]
fn shell_exec_node_eval_flag_executes_inline_code_without_vfs_read() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(capability(true, true), Arc::clone(&journal));

    let result = harness
        .cell
        .execute_shell(r#"node -e "console.log('inline node'); 21 + 21""#)
        .expect("node -e should execute inline QuickJS code");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stderr, "");
    assert_eq!(result.stdout, "inline node\n42\n");
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::CodeExecution { language } if language == "javascript"
            )
        }),
        "expected node -e to execute through the JS path"
    );
    assert!(
        !journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, .. } if tool_name == "read_file"
            )
        }),
        "node -e inline code must not try to read a script from VFS"
    );
}

#[test]
fn shell_exec_node_participates_in_shell_pipelines() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(capability(true, true), journal);
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write(
        "/workspace/script.js",
        b"console.log('alpha'); console.log('beta');",
    )
    .expect("seed script");

    let result = harness
        .cell
        .execute_shell("node /workspace/script.js | grep beta")
        .expect("node alias should run as a shell pipeline stage");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "beta\n");
    assert_eq!(result.stderr, "");
}

#[test]
fn shell_exec_node_dash_reads_script_from_pipeline_stdin() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(capability(true, true), Arc::clone(&journal));

    let result = harness
        .cell
        .execute_shell(r#"echo "console.log('from stdin')" | node -"#)
        .expect("node - should execute script piped on stdin");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "from stdin\n");
    assert!(
        !journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, .. } if tool_name == "read_file"
            )
        }),
        "node - must execute stdin without trying to read a script path"
    );
}

#[test]
fn shell_exec_node_output_redirect_uses_mediated_shell_vfs_write() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(capability(true, true), Arc::clone(&journal));

    let result = harness
        .cell
        .execute_shell(r#"node -e "console.log('to file')" > /workspace/out.txt"#)
        .expect("node alias output should be redirectable");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "");
    assert_eq!(
        harness.vfs.read("/workspace/out.txt").unwrap(),
        b"to file\n"
    );
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::FileWrite { path, size_bytes }
                if path == "/workspace/out.txt" && *size_bytes == 8
        )),
        "expected shell redirect to write through mediated VFS path"
    );
}

#[test]
fn shell_exec_nodejs_alias_matches_node_execution() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(capability(true, true), journal);
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write(
        "/workspace/script.js",
        br#"
        console.log("hello from alias");
        "done";
        "#,
    )
    .expect("seed script");

    let node = harness
        .cell
        .execute_shell("node /workspace/script.js")
        .expect("node alias should succeed");
    let nodejs = harness
        .cell
        .execute_shell("nodejs /workspace/script.js")
        .expect("nodejs alias should succeed");

    assert_eq!(nodejs.exit_code, node.exit_code);
    assert_eq!(nodejs.stdout, node.stdout);
    assert_eq!(nodejs.stderr, node.stderr);
}

#[cfg(feature = "python")]
#[test]
fn shell_exec_python_c_flag_executes_inline_code_without_vfs_read() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability_with_python(true, false, true),
        Arc::clone(&journal),
    );

    let result = harness
        .cell
        .execute_shell(r#"python -c "print('inline python')""#)
        .expect("python -c should execute inline Monty code");

    assert_eq!(result.exit_code, 0);
    assert!(
        result.stdout.contains("inline python"),
        "expected python stdout, got {:?}",
        result.stdout
    );
    assert!(
        !journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, .. } if tool_name == "read_file"
            )
        }),
        "python -c inline code must not try to read a script from VFS"
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == r#"python -c "print('inline python')""# && *exit_code == 0
            )
        }),
        "expected python -c execution to append a ShellCommand journal entry"
    );
}

#[cfg(feature = "python")]
#[test]
fn shell_exec_python_participates_in_shell_pipelines_and_redirects() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability_with_python(true, false, true),
        Arc::clone(&journal),
    );

    let result = harness
        .cell
        .execute_shell(
            r#"python -c "print('alpha'); print('beta')" | grep beta > /workspace/out.txt"#,
        )
        .expect("python alias should participate in shell pipeline and redirect stages");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "");
    assert_eq!(harness.vfs.read("/workspace/out.txt").unwrap(), b"beta\n");
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::FileWrite { path, size_bytes }
                if path == "/workspace/out.txt" && *size_bytes == 5
        )),
        "expected final redirect to write through mediated VFS path"
    );
}

#[cfg(feature = "python")]
#[test]
fn shell_exec_python_dash_reads_script_from_pipeline_stdin() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability_with_python(true, false, true),
        Arc::clone(&journal),
    );

    let result = harness
        .cell
        .execute_shell(r#"echo "print('from stdin')" | python -"#)
        .expect("python - should execute script piped on stdin");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "from stdin\n");
    assert!(
        !journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, .. } if tool_name == "read_file"
            )
        }),
        "python - must execute stdin without trying to read a script path"
    );
}

#[cfg(feature = "python")]
#[test]
fn shell_exec_python_alias_uses_mediated_external_functions() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability_with_python(true, false, true),
        Arc::clone(&journal),
    );

    let result = harness
        .cell
        .execute_shell(
            r#"python -c "write_file('/workspace/out.txt', 'created'); print(read_file('/workspace/out.txt'))""#,
        )
        .expect("python alias should use the mediated Monty dispatcher");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "created\n");
    assert_eq!(
        harness.vfs.read("/workspace/out.txt").unwrap(),
        b"created",
        "write_file bridge should write through AgentCell mediation"
    );
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::FileWrite { path, size_bytes }
                if path == "/workspace/out.txt" && *size_bytes == 7
        )),
        "expected mediated Python write to produce FileWrite journal entry"
    );
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::ToolResult { tool_name, is_error, .. }
                if tool_name == "read_file" && !is_error
        )),
        "expected mediated Python read to produce read_file ToolResult journal entry"
    );
}

#[test]
fn shell_exec_node_requires_javascript_capability() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(capability(true, false), journal);
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write("/workspace/script.js", b"console.log('blocked');")
        .expect("seed script");

    let result = harness
        .cell
        .execute_shell("node /workspace/script.js")
        .expect("node alias should surface JS capability denials as command results");

    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.contains("capability denied"),
        "expected JS capability denial in stderr, got {:?}",
        result.stderr
    );
}

#[test]
fn shell_exec_node_execution_is_journaled() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(capability(true, true), Arc::clone(&journal));
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write("/workspace/script.js", b"console.log('journaled');")
        .expect("seed script");

    let result = harness
        .cell
        .execute_shell("node /workspace/script.js")
        .expect("node alias should succeed");

    assert_eq!(result.exit_code, 0);
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::CodeExecution { language } if language == "javascript"
            )
        }),
        "expected node alias execution to append a JavaScript CodeExecution journal entry"
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, is_error, .. }
                    if tool_name == "read_file" && !is_error
            )
        }),
        "expected node alias to read the script through the mediated read_file path"
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "node /workspace/script.js" && *exit_code == 0
            )
        }),
        "expected node alias execution to append a ShellCommand journal entry"
    );
}

#[test]
fn shell_exec_node_script_read_is_mediated_by_paths_read_capability() {
    let journal = Arc::new(FakeJournalStorage::default());
    let capability = CapabilityToken {
        shell: true,
        javascript: true,
        paths_read: vec![],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..Default::default()
    };
    let harness = Harness::new(capability, Arc::clone(&journal));
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write("/workspace/script.js", b"console.log('blocked');")
        .expect("seed script");

    let result = harness
        .cell
        .execute_shell("node /workspace/script.js")
        .expect("node alias should surface script read denials as command results");

    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.contains("capability denied"),
        "expected mediated read denial in stderr, got {:?}",
        result.stderr
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "node /workspace/script.js" && *exit_code == 1
            )
        }),
        "expected failed node alias to append a ShellCommand journal entry"
    );
}

#[cfg(feature = "python")]
#[test]
fn shell_exec_python_execution_is_journaled_and_reads_script_through_mediated_path() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability_with_python(true, false, true),
        Arc::clone(&journal),
    );
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write("/workspace/script.py", b"print('hello from python')")
        .expect("seed script");

    let result = harness
        .cell
        .execute_shell("python3 /workspace/script.py")
        .expect("python alias should succeed");

    assert_eq!(result.exit_code, 0);
    assert!(
        result.stdout.contains("hello from python"),
        "expected python stdout, got {:?}",
        result.stdout
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, is_error, .. }
                    if tool_name == "read_file" && !is_error
            )
        }),
        "expected python alias to read the script through the mediated read_file path"
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "python3 /workspace/script.py" && *exit_code == 0
            )
        }),
        "expected python alias execution to append a ShellCommand journal entry"
    );
}

#[cfg(feature = "python")]
#[test]
fn shell_exec_python_script_read_is_mediated_by_paths_read_capability() {
    let journal = Arc::new(FakeJournalStorage::default());
    let capability = CapabilityToken {
        shell: true,
        python: true,
        paths_read: vec![],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..Default::default()
    };
    let harness = Harness::new(capability, Arc::clone(&journal));
    let fs: &dyn VirtualFs = harness.vfs.as_ref();
    fs.write("/workspace/script.py", b"print('blocked')")
        .expect("seed script");

    let result = harness
        .cell
        .execute_shell("python /workspace/script.py")
        .expect("python alias should surface script read denials as command results");

    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stdout, "");
    assert!(
        result.stderr.contains("capability denied"),
        "expected mediated read denial in stderr, got {:?}",
        result.stderr
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "python /workspace/script.py" && *exit_code == 1
            )
        }),
        "expected failed python alias to append a ShellCommand journal entry"
    );
}

#[test]
fn node_shell_alias_produces_the_same_operation_spans_as_execute_js() {
    let shell_journal = Arc::new(FakeJournalStorage::default());
    let js_journal = Arc::new(FakeJournalStorage::default());
    let shell_harness = Harness::new(capability(true, true), shell_journal);
    let js_harness = Harness::new(capability(true, true), js_journal);
    let fs: &dyn VirtualFs = shell_harness.vfs.as_ref();
    fs.write("/workspace/script.js", b"1 + 1")
        .expect("seed script");

    let (_, shell_spans) = capture_spans(|| {
        shell_harness
            .cell
            .execute_shell("node /workspace/script.js")
            .unwrap()
    });
    let (_, js_spans) = capture_spans(|| js_harness.cell.execute_js("1 + 1").unwrap());

    // The node alias goes through execute_shell and mediated read_file, so it
    // has those extra spans compared to direct execute_js.
    // But it must include the same JS execution spans.
    let js_ops = span_operations(&js_spans);
    let shell_ops = span_operations(&shell_spans);
    for op in &js_ops {
        assert!(
            shell_ops.contains(op),
            "node alias should include JS span '{op}', got: {shell_ops:?}"
        );
    }
    assert!(
        shell_ops.contains(&"sandbox_shell_exec".to_string()),
        "node alias should include sandbox_shell_exec span"
    );
    assert!(
        shell_ops.contains(&"sandbox_read_file".to_string()),
        "node alias should include sandbox_read_file span from reading the script file"
    );
}
