use rust_decimal::Decimal;
use serde_json::Value;
use simulacra_quickjs::JsOutput;
use simulacra_sandbox::{AgentCell, ScriptExecutor};
use simulacra_shell::CommandResult;
use simulacra_types::{
    AgentId, CapabilityDenied, CapabilityToken, CheckpointData, FsMetadata, JOURNAL_SCHEMA_VERSION,
    JournalEntry, JournalEntryKind, JournalError, JournalStorage, NetworkPermission, PathPattern,
    ResourceBudget, TokenUsage, VfsError, VfsSnapshot, VirtualFs,
};
use simulacra_vfs::MemoryFs;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc, Barrier, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;

#[allow(dead_code)]
#[derive(Debug)]
enum ExpectedSandboxError {
    CapabilityDenied(CapabilityDenied),
    BudgetExhausted {
        resource: String,
        used: String,
        limit: String,
    },
    Shell(String),
    Http(String),
    Js(String),
    Vfs(VfsError),
    Internal(String),
}

impl std::fmt::Display for ExpectedSandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapabilityDenied(denied) => write!(f, "{denied}"),
            Self::BudgetExhausted {
                resource,
                used,
                limit,
            } => write!(
                f,
                "budget exhausted: {resource} — used {used}, limit {limit}"
            ),
            Self::Shell(message)
            | Self::Http(message)
            | Self::Js(message)
            | Self::Internal(message) => {
                write!(f, "{message}")
            }
            Self::Vfs(error) => write!(f, "{error}"),
        }
    }
}

fn sandbox_error_to_expected(error: simulacra_sandbox::SandboxError) -> ExpectedSandboxError {
    match error {
        simulacra_sandbox::SandboxError::CapabilityDenied(denied) => {
            ExpectedSandboxError::CapabilityDenied(denied)
        }
        simulacra_sandbox::SandboxError::BudgetExhausted(exhausted) => {
            ExpectedSandboxError::BudgetExhausted {
                resource: exhausted.resource,
                used: exhausted.used,
                limit: exhausted.limit,
            }
        }
        simulacra_sandbox::SandboxError::Shell(message) => ExpectedSandboxError::Shell(message),
        simulacra_sandbox::SandboxError::Http(message) => ExpectedSandboxError::Http(message),
        simulacra_sandbox::SandboxError::Js(message) => ExpectedSandboxError::Js(message),
        simulacra_sandbox::SandboxError::Vfs(vfs_err) => ExpectedSandboxError::Vfs(vfs_err),
        simulacra_sandbox::SandboxError::Internal(message) => {
            ExpectedSandboxError::Internal(message)
        }
    }
}

#[derive(Default)]
struct SpyFs {
    inner: MemoryFs,
    reads: Mutex<Vec<String>>,
    writes: Mutex<Vec<(String, Vec<u8>)>>,
    lists: Mutex<Vec<String>>,
}

impl SpyFs {
    fn new() -> Self {
        Self::default()
    }

    fn seed_file(&self, path: &str, data: &[u8]) {
        self.inner
            .write(path, data)
            .expect("seed write should succeed");
    }

    fn clear_observations(&self) {
        self.reads.lock().unwrap().clear();
        self.writes.lock().unwrap().clear();
        self.lists.lock().unwrap().clear();
    }

    fn read_count(&self) -> usize {
        self.reads.lock().unwrap().len()
    }

    fn write_count(&self) -> usize {
        self.writes.lock().unwrap().len()
    }

    #[allow(dead_code)]
    fn list_count(&self) -> usize {
        self.lists.lock().unwrap().len()
    }
}

impl VirtualFs for SpyFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.reads.lock().unwrap().push(path.to_string());
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.writes
            .lock()
            .unwrap()
            .push((path.to_string(), data.to_vec()));
        self.inner.write(path, data)
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.lists.lock().unwrap().push(path.to_string());
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.inner.remove(path)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        self.inner.metadata(path)
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
}

struct SlowWriteFs {
    inner: MemoryFs,
    delay: Duration,
}

impl SlowWriteFs {
    fn new(delay: Duration) -> Self {
        Self {
            inner: MemoryFs::new(),
            delay,
        }
    }
}

impl VirtualFs for SlowWriteFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        thread::sleep(self.delay);
        self.inner.write(path, data)
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.inner.remove(path)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        self.inner.metadata(path)
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
}

struct PanicWriteFs {
    inner: MemoryFs,
}

impl PanicWriteFs {
    fn new() -> Self {
        Self {
            inner: MemoryFs::new(),
        }
    }
}

impl VirtualFs for PanicWriteFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }

    fn write(&self, path: &str, _data: &[u8]) -> Result<(), VfsError> {
        panic!("intentional panic while writing {path}");
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.inner.remove(path)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        self.inner.metadata(path)
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
}

#[derive(Debug, Default)]
struct FakeJournalStorage {
    entries: Mutex<Vec<JournalEntry>>,
    fail_next_append: AtomicBool,
}

impl FakeJournalStorage {
    fn entries(&self) -> Vec<JournalEntry> {
        self.entries.lock().unwrap().clone()
    }

    fn fail_next_append(&self) {
        self.fail_next_append.store(true, Ordering::SeqCst);
    }
}

impl JournalStorage for FakeJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        if self.fail_next_append.swap(false, Ordering::SeqCst) {
            return Err(JournalError::Storage("injected append failure".into()));
        }

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

#[derive(Debug, Clone)]
struct CapturedEvent {
    level: String,
    current_span: Option<String>,
    fields: HashMap<String, String>,
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
    span_fields: Arc<Mutex<HashMap<tracing::span::Id, HashMap<String, String>>>>,
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
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);
        self.span_fields
            .lock()
            .unwrap()
            .insert(id.clone(), fields.clone());
        self.spans.lock().unwrap().push(CapturedSpan { fields });
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut new_fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut new_fields);
        values.record(&mut visitor);

        let mut span_fields = self.span_fields.lock().unwrap();
        if let Some(existing) = span_fields.get_mut(id) {
            existing.extend(new_fields.clone());
        }

        // Update the matching span in the captured list
        let mut spans = self.spans.lock().unwrap();
        if let Some(sf) = span_fields.get(id) {
            // Find the span whose fields match the original (before record)
            // and update it with the new fields
            for span in spans.iter_mut().rev() {
                // Match by checking the span has the same base fields
                let is_match = sf
                    .iter()
                    .filter(|(k, _)| !new_fields.contains_key(k.as_str()))
                    .all(|(k, v)| span.fields.get(k) == Some(v));
                if is_match {
                    span.fields.extend(new_fields);
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

#[allow(clippy::type_complexity)]
fn setup_capture() -> (
    impl tracing::Subscriber + Send + Sync,
    Arc<Mutex<Vec<CapturedSpan>>>,
    Arc<Mutex<Vec<CapturedEvent>>>,
) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
        span_fields: Arc::new(Mutex::new(HashMap::new())),
        events: Arc::clone(&events),
    });
    (subscriber, spans, events)
}

fn capture_operation<R>(
    operation: impl FnOnce() -> R,
) -> (R, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    static GLOBAL_TRACING: OnceLock<()> = OnceLock::new();
    GLOBAL_TRACING.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
        tracing::callsite::rebuild_interest_cache();
    });

    static CAPTURE_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = CAPTURE_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("capture mutex should not be poisoned");
    let (subscriber, spans, events) = setup_capture();
    let result = tracing::subscriber::with_default(subscriber, operation);
    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

struct TestHttpServer {
    addr: String,
    request_count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestHttpServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        404 => "Not Found",
        _ => "OK",
    }
}

fn spawn_http_server(status: u16, headers: &[(&str, &str)], body: &[u8]) -> TestHttpServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
    listener
        .set_nonblocking(true)
        .expect("test server should become nonblocking");
    let addr = listener
        .local_addr()
        .expect("test server should expose an address")
        .to_string();
    let request_count = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let request_count_for_thread = Arc::clone(&request_count);
    let stop_for_thread = Arc::clone(&stop);
    let header_lines: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    let response_body = body.to_vec();

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    request_count_for_thread.fetch_add(1, Ordering::SeqCst);

                    let mut buffer = [0_u8; 4096];
                    let _ = stream.read(&mut buffer);

                    let mut response = format!(
                        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        status,
                        reason_phrase(status),
                        response_body.len()
                    );
                    for (name, value) in &header_lines {
                        response.push_str(name);
                        response.push_str(": ");
                        response.push_str(value);
                        response.push_str("\r\n");
                    }
                    response.push_str("\r\n");

                    stream
                        .write_all(response.as_bytes())
                        .expect("test server should write response headers");
                    stream
                        .write_all(&response_body)
                        .expect("test server should write response body");
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    TestHttpServer {
        addr,
        request_count,
        stop,
        handle: Some(handle),
    }
}

struct Harness {
    vfs: Arc<SpyFs>,
    cell: AgentCell,
}

impl Harness {
    fn new(
        capability: CapabilityToken,
        budget: Arc<Mutex<ResourceBudget>>,
        journal: Arc<FakeJournalStorage>,
    ) -> Self {
        let vfs = Arc::new(SpyFs::new());
        let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
        let journal_dyn: Arc<dyn JournalStorage> = journal.clone();
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        let cell = AgentCell::new(
            Arc::clone(&vfs_dyn),
            capability,
            Arc::clone(&budget),
            journal_dyn,
            http_client,
        );

        Self { vfs, cell }
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, ExpectedSandboxError> {
        self.cell.read_file(path).map_err(sandbox_error_to_expected)
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), ExpectedSandboxError> {
        self.cell
            .write_file(path, data)
            .map_err(sandbox_error_to_expected)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, ExpectedSandboxError> {
        self.cell.list_dir(path).map_err(sandbox_error_to_expected)
    }

    fn execute_shell(&self, command: &str) -> Result<CommandResult, ExpectedSandboxError> {
        self.cell
            .execute_shell(command)
            .map_err(sandbox_error_to_expected)
    }

    fn execute_js(&self, code: &str) -> Result<JsOutput, ExpectedSandboxError> {
        self.cell
            .execute_js(code)
            .map_err(sandbox_error_to_expected)
    }
}

fn capability(reads: &[&str], writes: &[&str], shell: bool, javascript: bool) -> CapabilityToken {
    capability_with_network(reads, writes, &[], shell, javascript)
}

fn capability_with_network(
    reads: &[&str],
    writes: &[&str],
    network: &[&str],
    shell: bool,
    javascript: bool,
) -> CapabilityToken {
    CapabilityToken {
        network: network
            .iter()
            .map(|permission| NetworkPermission((*permission).to_string()))
            .collect(),
        shell,
        javascript,
        paths_read: reads
            .iter()
            .map(|pattern| PathPattern((*pattern).to_string()))
            .collect(),
        paths_write: writes
            .iter()
            .map(|pattern| PathPattern((*pattern).to_string()))
            .collect(),
        ..Default::default()
    }
}

fn unlimited_budget() -> Arc<Mutex<ResourceBudget>> {
    Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0)))
}

fn budget_with_overrides(
    max_turns: u32,
    used_turns: u32,
    max_vfs_bytes: u64,
    used_vfs_bytes: u64,
) -> Arc<Mutex<ResourceBudget>> {
    let mut value = serde_json::to_value(ResourceBudget::new(0, max_turns, Decimal::ZERO, 0))
        .expect("budget should serialize");
    let map = value
        .as_object_mut()
        .expect("resource budget should serialize as an object");
    map.insert("used_turns".into(), Value::from(used_turns));
    map.insert("max_vfs_bytes".into(), Value::from(max_vfs_bytes));
    map.insert("used_vfs_bytes".into(), Value::from(used_vfs_bytes));
    Arc::new(Mutex::new(
        serde_json::from_value(value).expect("budget should deserialize"),
    ))
}

fn budget_counter(budget: &Arc<Mutex<ResourceBudget>>, field: &str) -> u64 {
    serde_json::to_value(&*budget.lock().unwrap())
        .expect("budget should serialize")
        .get(field)
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn journal_payload(entry: &JournalEntry) -> String {
    serde_json::to_string(&entry.entry).expect("journal entry should serialize")
}

fn assert_budget_exhausted(
    error: ExpectedSandboxError,
    expected_resources: &[&str],
    used: &str,
    limit: &str,
) {
    match error {
        ExpectedSandboxError::BudgetExhausted {
            resource,
            used: actual_used,
            limit: actual_limit,
        } => {
            assert!(
                expected_resources.contains(&resource.as_str()),
                "expected one of {expected_resources:?}, got {resource}"
            );
            assert_eq!(actual_used, used);
            assert_eq!(actual_limit, limit);
        }
        other => panic!("expected BudgetExhausted, got {other:?}"),
    }
}

#[test]
fn read_file_with_denied_paths_read_returns_capability_denied_and_does_not_touch_vfs() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/foo.txt", b"secret");
    harness.vfs.clear_observations();

    let error = harness.read_file("/workspace/foo.txt").unwrap_err();

    assert!(matches!(
        error,
        ExpectedSandboxError::CapabilityDenied(CapabilityDenied { operation, .. }) if operation == "read_file"
    ));
    assert_eq!(
        harness.vfs.read_count(),
        0,
        "denied reads must not hit the VFS"
    );
}

#[test]
fn read_file_with_denied_paths_read_surfaces_operation_and_reason_to_agent() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/foo.txt", b"secret");

    let error = harness.read_file("/workspace/foo.txt").unwrap_err();

    match error {
        ExpectedSandboxError::CapabilityDenied(denied) => {
            assert_eq!(denied.operation, "read_file");
            assert_eq!(denied.reason, "read access denied for /workspace/foo.txt");
        }
        other => panic!("expected capability denial, got {other:?}"),
    }
}

#[test]
fn write_file_with_denied_paths_write_returns_capability_denied_and_does_not_write() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .write_file("/workspace/output.txt", b"hello")
        .unwrap_err();

    assert!(matches!(
        error,
        ExpectedSandboxError::CapabilityDenied(CapabilityDenied { operation, .. }) if operation == "write_file"
    ));
    assert_eq!(
        harness.vfs.write_count(),
        0,
        "denied writes must not hit the VFS"
    );
    assert!(!harness.vfs.exists("/workspace/output.txt"));
}

#[test]
fn execute_shell_with_shell_false_returns_capability_denied_and_does_not_execute() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_shell("echo blocked > /workspace/blocked.txt")
        .unwrap_err();

    assert!(matches!(error, ExpectedSandboxError::CapabilityDenied(_)));
    assert!(!harness.vfs.exists("/workspace/blocked.txt"));
}

#[test]
fn execute_js_with_javascript_false_returns_capability_denied_and_does_not_execute_js() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_js("fs.writeFileSync('/workspace/blocked.txt', 'x')")
        .unwrap_err();

    assert!(matches!(error, ExpectedSandboxError::CapabilityDenied(_)));
    assert!(!harness.vfs.exists("/workspace/blocked.txt"));
}

#[test]
fn execute_js_with_javascript_false_surfaces_operation_and_reason_to_agent() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_js("fs.writeFileSync('/workspace/blocked.txt', 'x')")
        .unwrap_err();

    match error {
        ExpectedSandboxError::CapabilityDenied(denied) => {
            assert_eq!(denied.operation, "javascript");
            assert_eq!(denied.reason, "javascript capability not granted");
        }
        other => panic!("expected capability denial, got {other:?}"),
    }
}

#[test]
fn write_file_when_vfs_bytes_budget_is_exhausted_returns_budget_exhausted_and_does_not_write() {
    let harness = Harness::new(
        capability(&[], &["/workspace/output.txt"], false, false),
        budget_with_overrides(0, 0, 1, 1),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .write_file("/workspace/output.txt", b"hello")
        .unwrap_err();

    assert_budget_exhausted(error, &["vfs_bytes"], "1", "1");
    assert!(!harness.vfs.exists("/workspace/output.txt"));
}

#[test]
fn write_file_that_would_exceed_vfs_bytes_budget_is_rejected_before_write() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &["/workspace/output.txt"], false, false),
        budget_with_overrides(0, 0, 1, 0),
        Arc::clone(&journal),
    );

    let error = harness
        .write_file("/workspace/output.txt", b"hello")
        .unwrap_err();

    assert_budget_exhausted(error, &["vfs_bytes"], "5", "1");
    assert!(!harness.vfs.exists("/workspace/output.txt"));
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::ToolResult { tool_name, is_error, .. }
                if tool_name == "vfs_bytes" && *is_error
        )),
        "expected boundary-crossing write to journal budget exhaustion"
    );
}

#[test]
fn concurrent_write_file_reserves_vfs_bytes_without_overshooting_limit() {
    let budget = budget_with_overrides(0, 0, 10, 0);
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let vfs: Arc<dyn VirtualFs> = Arc::new(SlowWriteFs::new(Duration::from_millis(50)));
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = Arc::new(AgentCell::new(
        vfs,
        capability(&[], &["/workspace/**"], false, false),
        Arc::clone(&budget),
        journal,
        http_client,
    ));
    let barrier = Arc::new(Barrier::new(3));

    let mut handles = Vec::new();
    for idx in 0..2 {
        let cell = Arc::clone(&cell);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            cell.write_file(&format!("/workspace/{idx}.txt"), b"123456")
        }));
    }

    barrier.wait();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("writer thread should not panic"))
        .collect::<Vec<_>>();

    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one 6-byte write should fit into a 10-byte VFS budget: {results:?}"
    );
    assert_eq!(
        results.iter().filter(|result| result.is_err()).count(),
        1,
        "one concurrent write must be rejected before overshooting the budget: {results:?}"
    );
    assert_eq!(budget_counter(&budget, "used_vfs_bytes"), 6);
}

#[test]
fn concurrent_execute_shell_reserves_turns_without_overshooting_limit() {
    let budget = budget_with_overrides(1, 0, 0, 0);
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let vfs: Arc<dyn VirtualFs> = Arc::new(SlowWriteFs::new(Duration::from_millis(50)));
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = Arc::new(AgentCell::new(
        vfs,
        capability(&[], &["/workspace/**"], true, false),
        Arc::clone(&budget),
        journal,
        http_client,
    ));
    let barrier = Arc::new(Barrier::new(3));

    let mut handles = Vec::new();
    for idx in 0..2 {
        let cell = Arc::clone(&cell);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            cell.execute_shell(&format!("echo hi > /workspace/{idx}.txt"))
        }));
    }

    barrier.wait();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("shell thread should not panic"))
        .collect::<Vec<_>>();

    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one shell command should reserve the single available turn: {results:?}"
    );
    assert_eq!(
        results.iter().filter(|result| result.is_err()).count(),
        1,
        "one concurrent shell command must be rejected before overshooting turns: {results:?}"
    );
    assert_eq!(budget_counter(&budget, "used_turns"), 1);
}

#[test]
fn execute_shell_when_tool_calls_budget_is_exhausted_returns_budget_exhausted_and_does_not_execute()
{
    let harness = Harness::new(
        capability(&[], &[], true, false),
        budget_with_overrides(1, 1, 0, 0),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_shell("echo blocked > /workspace/blocked.txt")
        .unwrap_err();

    assert_budget_exhausted(error, &["tool_calls", "turns"], "1", "1");
    assert!(!harness.vfs.exists("/workspace/blocked.txt"));
}

#[test]
fn execute_js_when_tool_calls_budget_is_exhausted_returns_budget_exhausted_and_does_not_execute() {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        budget_with_overrides(1, 1, 0, 0),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness.execute_js("1 + 1").unwrap_err();

    assert_budget_exhausted(error, &["tool_calls", "turns"], "1", "1");
}

#[test]
fn execute_js_respects_configured_script_executor_permit() {
    let executor = ScriptExecutor::new(1);
    let _held_permit = executor
        .try_acquire_permit()
        .expect("test should reserve the only script permit");
    let vfs = Arc::new(SpyFs::new());
    let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
    let budget = unlimited_budget();
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let mut cell = AgentCell::new(
        vfs_dyn,
        capability(&[], &[], false, true),
        budget,
        journal,
        http_client,
    );
    cell.set_script_executor(executor.clone());

    let error = cell.execute_js("1 + 1").unwrap_err();

    match sandbox_error_to_expected(error) {
        ExpectedSandboxError::Internal(message) => assert!(
            message.contains("script executor permit unavailable"),
            "unexpected internal error: {message}"
        ),
        other => panic!("expected script executor internal error, got {other:?}"),
    }
}

#[test]
fn write_file_writes_a_filewrite_journal_entry_with_path_and_size_before_returning() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &["/output/result.txt"], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    harness
        .write_file("/output/result.txt", b"abc")
        .expect("write should succeed once the proxy exists");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::FileWrite { path, size_bytes }
                    if path == "/output/result.txt" && *size_bytes == 3
            )
        }),
        "expected a FileWrite journal entry for the proxied VFS write"
    );
}

#[test]
fn execute_shell_writes_a_shellcommand_journal_entry_with_command_and_exit_code() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &[], true, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    let result = harness
        .execute_shell("echo hello")
        .expect("shell command should succeed once proxied");

    assert_eq!(result.exit_code, 0);
    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "echo hello" && *exit_code == 0
            )
        }),
        "expected a ShellCommand journal entry with command and exit code"
    );
}

#[test]
fn execute_shell_cat_read_is_mediated_by_paths_read_capability() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &["/workspace/**"], true, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/secret.txt", b"secret");
    harness.vfs.clear_observations();

    let result = harness
        .execute_shell("cat /workspace/secret.txt")
        .expect("shell should surface mediated read denial as command result");

    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.contains("permission denied") || result.stderr.contains("capability denied"),
        "expected mediated read denial, got {:?}",
        result.stderr
    );
    assert_eq!(
        harness.vfs.read_count(),
        0,
        "capability denial must happen before the shell touches VFS read"
    );
    assert!(
        journal.entries().iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "cat /workspace/secret.txt" && *exit_code != 0
            )
        }),
        "expected denied shell command to still be journaled"
    );
}

#[test]
fn execute_shell_redirect_write_is_mediated_by_paths_write_capability() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], true, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness.vfs.clear_observations();

    let result = harness
        .execute_shell("echo blocked > /workspace/blocked.txt")
        .expect("shell should surface mediated write denial as command result");

    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.contains("permission denied") || result.stderr.contains("capability denied"),
        "expected mediated write denial, got {:?}",
        result.stderr
    );
    assert_eq!(
        harness.vfs.write_count(),
        0,
        "capability denial must happen before the shell touches VFS write"
    );
    assert!(!harness.vfs.exists("/workspace/blocked.txt"));
}

#[test]
fn execute_js_writes_a_codeexecution_journal_entry_with_language_javascript() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    let _ = harness.execute_js("1 + 1");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::CodeExecution { language } if language == "javascript"
            )
        }),
        "expected a CodeExecution journal entry for JavaScript execution"
    );
}

#[test]
fn capability_denial_writes_a_journal_entry_recording_the_denied_operation_and_reason() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    let _ = harness.write_file("/workspace/denied.txt", b"denied");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            let payload = journal_payload(entry);
            payload.contains("write_file") && payload.contains("denied")
        }),
        "expected a journal entry recording the denied operation and reason"
    );
}

#[test]
fn budget_exhaustion_writes_a_journal_entry_recording_the_exhausted_resource() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &[], true, false),
        budget_with_overrides(1, 1, 0, 0),
        Arc::clone(&journal),
    );

    let _ = harness.execute_shell("echo blocked");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            let payload = journal_payload(entry);
            payload.contains("tool_calls") || payload.contains("turns")
        }),
        "expected a journal entry recording the exhausted budget resource"
    );
}

#[test]
fn execute_shell_increments_used_tool_calls_by_one() {
    let budget = unlimited_budget();
    let harness = Harness::new(
        capability(&[], &[], true, false),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    harness
        .execute_shell("echo hello")
        .expect("shell command should succeed");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after,
        before + 1,
        "execute_shell must consume one tool-call unit"
    );
}

#[test]
fn execute_js_increments_used_tool_calls_by_one() {
    let budget = unlimited_budget();
    let harness = Harness::new(
        capability(&[], &[], false, true),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    let _ = harness.execute_js("1 + 1");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after,
        before + 1,
        "execute_js must consume one tool-call unit"
    );
}

#[test]
fn write_file_increments_used_vfs_bytes_by_the_written_byte_count() {
    let budget = budget_with_overrides(0, 0, 1024, 0);
    let harness = Harness::new(
        capability(&[], &["/output/result.txt"], false, false),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_vfs_bytes");

    harness
        .write_file("/output/result.txt", b"hello")
        .expect("write should succeed");

    let after = budget_counter(&budget, "used_vfs_bytes");
    assert_eq!(after, before + 5, "write_file must consume VFS byte budget");
}

#[test]
fn zero_budget_limits_are_treated_as_unlimited() {
    let budget = budget_with_overrides(0, 99, 0, 2048);
    let harness = Harness::new(
        capability(&[], &["/output/result.txt"], true, false),
        budget,
        Arc::new(FakeJournalStorage::default()),
    );

    harness
        .execute_shell("echo still-allowed")
        .expect("tool-call budget limit 0 should be unlimited");
    harness
        .write_file("/output/result.txt", b"still allowed")
        .expect("vfs-bytes budget limit 0 should be unlimited");
}

#[test]
fn workspace_read_glob_allows_reading_a_workspace_file() {
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/foo.txt", b"workspace");

    let data = harness
        .read_file("/workspace/foo.txt")
        .expect("workspace glob should allow reading files under /workspace");

    assert_eq!(data, b"workspace");
}

#[test]
fn workspace_read_glob_denies_reading_a_secret_outside_workspace() {
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/secrets/key.pem", b"secret");

    let error = harness.read_file("/secrets/key.pem").unwrap_err();

    assert!(matches!(error, ExpectedSandboxError::CapabilityDenied(_)));
}

#[test]
fn output_write_glob_allows_writing_under_output() {
    let harness = Harness::new(
        capability(&[], &["/output/**"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    harness
        .write_file("/output/result.txt", b"ok")
        .expect("output glob should allow writing under /output");

    assert!(harness.vfs.exists("/output/result.txt"));
}

#[test]
fn output_write_glob_denies_writing_outside_output() {
    let harness = Harness::new(
        capability(&[], &["/output/**"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .write_file("/workspace/sneaky.txt", b"nope")
        .unwrap_err();

    assert!(matches!(error, ExpectedSandboxError::CapabilityDenied(_)));
    assert!(!harness.vfs.exists("/workspace/sneaky.txt"));
}

#[test]
fn wildcard_root_read_pattern_allows_reading_any_path() {
    let harness = Harness::new(
        capability(&["/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/secrets/key.pem", b"secret");

    let data = harness
        .read_file("/secrets/key.pem")
        .expect("root wildcard should allow reading any path");

    assert_eq!(data, b"secret");
}

#[test]
fn empty_path_capabilities_deny_all_reads_and_writes() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/foo.txt", b"read me");

    let read_error = harness.read_file("/workspace/foo.txt").unwrap_err();
    let write_error = harness
        .write_file("/workspace/bar.txt", b"write me")
        .unwrap_err();

    assert!(matches!(
        read_error,
        ExpectedSandboxError::CapabilityDenied(_)
    ));
    assert!(matches!(
        write_error,
        ExpectedSandboxError::CapabilityDenied(_)
    ));
}

// ---------------------------------------------------------------------------
// SB8: Path-capability security edges (traversal, normalization)
// ---------------------------------------------------------------------------

#[test]
fn path_traversal_starting_outside_allowed_prefix_is_denied() {
    // A path that uses .. to escape from the root — it never starts with /workspace
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/etc/passwd", b"root:x:0:0");

    let error = harness.read_file("/etc/passwd").unwrap_err();

    assert!(
        matches!(error, ExpectedSandboxError::CapabilityDenied(_)),
        "reading a path outside the allowed prefix must be denied, got {error:?}"
    );
}

#[test]
fn relative_path_without_allowed_prefix_is_denied() {
    // Relative paths that don't start with the allowed absolute prefix are denied
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness.read_file("../secret/data.txt").unwrap_err();

    assert!(
        matches!(error, ExpectedSandboxError::CapabilityDenied(_)),
        "relative path traversal must be denied, got {error:?}"
    );
}

#[test]
fn write_to_path_outside_allowed_prefix_is_denied() {
    let harness = Harness::new(
        capability(&[], &["/output/**"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .write_file("/etc/crontab", b"malicious")
        .unwrap_err();

    assert!(
        matches!(error, ExpectedSandboxError::CapabilityDenied(_)),
        "writing outside the allowed prefix must be denied, got {error:?}"
    );
    assert!(
        !harness.vfs.exists("/etc/crontab"),
        "denied write must not create files outside the allowed path"
    );
}

#[test]
fn exact_path_capability_does_not_allow_sibling_paths() {
    let harness = Harness::new(
        capability(&["/workspace/allowed.txt"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/secret.txt", b"secret");

    let error = harness.read_file("/workspace/secret.txt").unwrap_err();

    assert!(
        matches!(error, ExpectedSandboxError::CapabilityDenied(_)),
        "exact path capability must not allow sibling files, got {error:?}"
    );
}

#[test]
fn agent_cell_new_accepts_vfs_capability_budget_and_journal() {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let capability = CapabilityToken::default();
    let budget = unlimited_budget();
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());

    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let _cell = AgentCell::new(vfs, capability, budget, journal, http_client);
}

#[test]
fn agent_cell_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<AgentCell>();
}

#[test]
fn two_agent_cells_with_different_vfs_references_do_not_share_filesystem_state() {
    let left = Harness::new(
        capability(&[], &["/workspace/shared.txt"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    let right = Harness::new(
        capability(&[], &["/workspace/shared.txt"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    left.write_file("/workspace/shared.txt", b"left")
        .expect("left write should succeed");

    assert!(left.vfs.exists("/workspace/shared.txt"));
    assert!(
        !right.vfs.exists("/workspace/shared.txt"),
        "separate cells must not share VFS state"
    );
}

#[test]
fn agent_cell_holds_a_persistent_shellexecutor_so_shell_state_survives_across_execute_shell_calls()
{
    let harness = Harness::new(
        capability(&[], &[], true, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let first = harness
        .execute_shell("export GREETING=hello-from-shell")
        .expect("persistent shell executor should accept exporting state");
    assert_eq!(
        first.exit_code, 0,
        "export should succeed so shell state can persist across calls"
    );

    let second = harness
        .execute_shell("echo $GREETING")
        .expect("second shell call should observe the exported environment");
    assert_eq!(
        second.stdout.trim(),
        "hello-from-shell",
        "shell environment variables should survive across execute_shell calls"
    );
}

#[test]
fn agent_cell_js_exec_does_not_leak_global_state_across_calls() {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    harness
        .execute_js("globalThis.__simulacra_counter = 41; Object.prototype.polluted = true;")
        .expect("first JS execution should run");

    let output = harness
        .execute_js(
            r#"
            [
              typeof globalThis.__simulacra_counter,
              Object.prototype.polluted === true
            ].join("|")
            "#,
        )
        .expect("second JS execution should run in a fresh context");
    assert_eq!(
        output.result.as_deref(),
        Some("undefined|false"),
        "JS globals must not survive across execute_js calls within the same AgentCell"
    );
}

#[test]
fn vfs_errors_propagate_as_sandbox_vfs_errors() {
    let harness = Harness::new(
        capability(&["/missing.txt"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness.read_file("/missing.txt").unwrap_err();

    assert!(matches!(
        error,
        ExpectedSandboxError::Vfs(VfsError::NotFound(path)) if path == "/missing.txt"
    ));
}

#[test]
fn shell_command_not_found_returns_a_shell_result_not_a_sandbox_error() {
    let harness = Harness::new(
        capability(&[], &[], true, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let result = harness
        .execute_shell("nonexistent_cmd")
        .expect("shell errors should surface as CommandResult");

    assert_ne!(
        result.exit_code, 0,
        "a missing command must produce a non-zero exit code"
    );
    assert!(
        !result.stderr.is_empty(),
        "a missing command must produce some diagnostic on stderr"
    );
}

#[test]
fn journal_write_failure_does_not_prevent_execution_and_is_logged_at_error() {
    let journal = Arc::new(FakeJournalStorage::default());
    journal.fail_next_append();
    let harness = Harness::new(
        capability(&[], &["/output/result.txt"], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    let (result, _spans, events) =
        capture_operation(|| harness.write_file("/output/result.txt", b"ok"));

    result.expect("the VFS write should still succeed when the journal backend fails");
    assert!(harness.vfs.exists("/output/result.txt"));
    assert!(
        events.iter().any(|event| {
            event.level == "ERROR"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("journal") || value.contains("append"))
        }),
        "expected an ERROR log for the journal append failure"
    );
}

#[test]
fn read_file_produces_a_sandbox_read_file_span_with_vfs_path() {
    let harness = Harness::new(
        capability(&["/workspace/foo.txt"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/foo.txt", b"span me");

    let (_result, spans, _events) = capture_operation(|| harness.read_file("/workspace/foo.txt"));

    assert!(
        spans.iter().any(|span| {
            span.fields.get("simulacra.operation.name") == Some(&"sandbox_read_file".to_string())
                && span.fields.get("simulacra.vfs.path") == Some(&"/workspace/foo.txt".to_string())
        }),
        "expected sandbox_read_file span with simulacra.vfs.path"
    );
}

#[test]
fn write_file_produces_a_sandbox_write_file_span_with_path_and_bytes() {
    let harness = Harness::new(
        capability(&[], &["/output/result.txt"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let (_result, spans, _events) =
        capture_operation(|| harness.write_file("/output/result.txt", b"hello"));

    assert!(
        spans.iter().any(|span| {
            span.fields.get("simulacra.operation.name") == Some(&"sandbox_write_file".to_string())
                && span.fields.get("simulacra.vfs.path") == Some(&"/output/result.txt".to_string())
                && span.fields.get("simulacra.vfs.bytes") == Some(&"5".to_string())
        }),
        "expected sandbox_write_file span with simulacra.vfs.path and simulacra.vfs.bytes"
    );
}

#[test]
fn execute_shell_produces_a_sandbox_shell_exec_span_with_command() {
    let harness = Harness::new(
        capability(&[], &[], true, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let (_result, spans, _events) = capture_operation(|| harness.execute_shell("echo hello"));

    assert!(
        spans.iter().any(|span| {
            span.fields.get("simulacra.operation.name") == Some(&"sandbox_shell_exec".to_string())
                && span.fields.get("simulacra.shell.command") == Some(&"echo hello".to_string())
        }),
        "expected sandbox_shell_exec span with simulacra.shell.command"
    );
}

#[test]
fn execute_js_produces_a_sandbox_js_exec_span() {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let (_result, spans, _events) = capture_operation(|| harness.execute_js("1 + 1"));

    assert!(
        spans.iter().any(|span| {
            span.fields.get("simulacra.operation.name") == Some(&"sandbox_js_exec".to_string())
        }),
        "expected sandbox_js_exec span"
    );
}

#[test]
fn capability_denials_emit_warn_events_with_operation_and_reason_on_the_current_span() {
    let harness = Harness::new(
        capability(&[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let (_result, _spans, events) =
        capture_operation(|| harness.write_file("/workspace/denied.txt", b"denied"));

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event.current_span.is_some()
                && event.fields.get("simulacra.capability.operation")
                    == Some(&"write_file".to_string())
                && event
                    .fields
                    .get("simulacra.capability.reason")
                    .map(|value| value.contains("denied"))
                    .unwrap_or(false)
        }),
        "expected a WARN event on the current span for capability denial"
    );
}

#[test]
fn capability_denials_increment_counter_with_operation_labels_for_each_denial_type() {
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"denied");
    let harness = Harness::new(
        capability_with_network(&[], &[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/secret.txt", b"secret");

    let (_result, _spans, events) = capture_operation(|| {
        let _ = harness.write_file("/workspace/denied.txt", b"blocked");
        let _ = harness.read_file("/workspace/secret.txt");
        let _ = harness.execute_shell("echo blocked");
        let _ = harness.execute_js("1 + 1");
        let _ = harness
            .cell
            .fetch_http(&server.url("/blocked"), "GET", &[], None, None);
    });

    let denial_counter_events = events
        .iter()
        .filter(|event| event.fields.get("simulacra.capability.denials") == Some(&"1".to_string()))
        .collect::<Vec<_>>();

    assert_eq!(
        denial_counter_events.len(),
        5,
        "expected one simulacra.capability.denials counter increment per denied operation, got {events:#?}"
    );

    for expected_operation in [
        "paths_write",
        "paths_read",
        "shell",
        "javascript",
        "network",
    ] {
        assert!(
            denial_counter_events.iter().any(|event| {
                event.fields.get("operation") == Some(&expected_operation.to_string())
            }),
            "expected simulacra.capability.denials counter event labeled with operation={expected_operation}, got {events:#?}"
        );
    }
}

#[test]
fn budget_exhaustion_emits_warn_events_with_resource_used_and_limit_on_the_current_span() {
    let harness = Harness::new(
        capability(&[], &[], true, false),
        budget_with_overrides(1, 1, 0, 0),
        Arc::new(FakeJournalStorage::default()),
    );

    let (_result, _spans, events) = capture_operation(|| harness.execute_shell("echo blocked"));

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event.current_span.is_some()
                && event
                    .fields
                    .get("simulacra.budget.resource")
                    .map(|value| value == "tool_calls" || value == "turns")
                    .unwrap_or(false)
                && event.fields.get("simulacra.budget.used") == Some(&"1".to_string())
                && event.fields.get("simulacra.budget.limit") == Some(&"1".to_string())
        }),
        "expected a WARN event on the current span for budget exhaustion"
    );
}

#[test]
fn list_dir_checks_paths_read_budget_and_delegates_to_vfs() {
    let harness = Harness::new(
        capability(&["/workspace"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/a.txt", b"a");
    harness.vfs.seed_file("/workspace/b.txt", b"b");

    let entries = harness
        .list_dir("/workspace")
        .expect("list_dir should proxy to the VFS once implemented");

    assert_eq!(entries, vec!["a.txt".to_string(), "b.txt".to_string()]);
}

#[test]
fn fetch_http_with_denied_network_capability_returns_capability_denied_and_does_not_make_a_request()
{
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"ok");
    let harness = Harness::new(
        capability_with_network(&[], &[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .cell
        .fetch_http(&server.url("/denied"), "GET", &[], None, None)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("capability denied") && error.contains("127.0.0.1"),
        "expected denied network fetch error, got {error}"
    );
    assert_eq!(
        server.request_count(),
        0,
        "denied network fetches must not hit the HTTP client"
    );
}

#[test]
fn fetch_http_with_denied_network_capability_surfaces_operation_and_reason_to_agent() {
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"blocked");
    let harness = Harness::new(
        capability_with_network(&[], &[], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .cell
        .fetch_http(&server.url("/blocked"), "GET", &[], None, None)
        .unwrap_err();

    match sandbox_error_to_expected(error) {
        ExpectedSandboxError::CapabilityDenied(denied) => {
            assert_eq!(denied.operation, "network:127.0.0.1");
            assert_eq!(denied.reason, "no network permission for 127.0.0.1");
        }
        other => panic!("expected capability denial, got {other:?}"),
    }
}

#[test]
fn fetch_http_when_turns_budget_is_exhausted_returns_budget_exhausted_and_does_not_make_a_request()
{
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"ok");
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        budget_with_overrides(1, 1, 0, 0),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .cell
        .fetch_http(&server.url("/budget"), "GET", &[], None, None)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("budget exhausted") && error.contains("turns"),
        "expected turns budget exhaustion, got {error}"
    );
    assert_eq!(
        server.request_count(),
        0,
        "budget exhaustion must short-circuit before the HTTP request starts"
    );
}

#[test]
fn fetch_http_writes_an_httprequest_journal_entry_with_method_url_and_status_after_execution() {
    let server = spawn_http_server(201, &[("content-type", "text/plain")], b"created");
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    let url = server.url("/journal");

    let response = harness
        .cell
        .fetch_http(&url, "GET", &[], None, None)
        .expect("HTTP fetch should succeed for an allowed host");

    assert_eq!(response.status, 201);
    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::HttpRequest { method, url: entry_url, status }
                    if method == "GET" && entry_url == &url && *status == 201
            )
        }),
        "expected an HttpRequest journal entry with method, URL, and status"
    );
}

#[test]
fn fetch_http_increments_used_turns_by_one() {
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"ok");
    let budget = unlimited_budget();
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    harness
        .cell
        .fetch_http(&server.url("/turns"), "GET", &[], None, None)
        .expect("HTTP fetch should consume one turn");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after,
        before + 1,
        "fetch_http must consume one turns budget unit"
    );
}

#[test]
fn list_dir_does_not_increment_used_turns() {
    let budget = budget_with_overrides(5, 2, 0, 0);
    let harness = Harness::new(
        capability(&["/workspace"], &[], false, false),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/a.txt", b"a");
    let before = budget_counter(&budget, "used_turns");

    harness
        .list_dir("/workspace")
        .expect("list_dir should not consume a turns budget unit");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after, before,
        "list_dir is metadata-only and must not increment used_turns"
    );
}

#[test]
fn execute_shell_execute_js_and_fetch_http_all_increment_used_turns_before_execution_not_after() {
    let shell_budget = unlimited_budget();
    let shell_vfs: Arc<dyn VirtualFs> = Arc::new(PanicWriteFs::new());
    let shell_http: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let shell_cell = AgentCell::new(
        Arc::clone(&shell_vfs),
        capability(&[], &["/workspace/**"], true, false),
        Arc::clone(&shell_budget),
        Arc::new(FakeJournalStorage::default()),
        shell_http,
    );

    let shell_result = catch_unwind(AssertUnwindSafe(|| {
        let _ = shell_cell.execute_shell("echo boom > /workspace/panic.txt");
    }));
    assert!(
        shell_result.is_err(),
        "the panic-on-write VFS should interrupt shell execution"
    );
    assert_eq!(
        budget_counter(&shell_budget, "used_turns"),
        1,
        "execute_shell must pay its turns cost before execution starts, even if execution panics"
    );

    let js_budget = budget_with_overrides(1, 0, 0, 0);
    let js_harness = Harness::new(
        capability_with_network(&[], &[], &["net:modules.invalid"], false, true),
        Arc::clone(&js_budget),
        Arc::new(FakeJournalStorage::default()),
    );

    let js_error = js_harness
        .execute_js(
            r#"
            import value from "https://modules.invalid/entry.js";
            value;
            "#,
        )
        .expect_err(
            "execute_js should consume the only turns budget unit before module loading begins",
        );
    assert_budget_exhausted(js_error, &["turns"], "1", "1");

    let http_budget = unlimited_budget();
    let http_harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        Arc::clone(&http_budget),
        Arc::new(FakeJournalStorage::default()),
    );

    let http_error = http_harness
        .cell
        .fetch_http("http://127.0.0.1:9/before-exec", "GET", &[], None, None)
        .expect_err("connection-refused fetch should still consume a turns budget unit");
    let http_error_text = http_error.to_string();
    assert!(
        http_error_text.contains("127.0.0.1:9"),
        "HTTP errors should still report the failed URL, got {http_error_text}"
    );
    assert_eq!(
        budget_counter(&http_budget, "used_turns"),
        1,
        "fetch_http must pay its turns cost before the request executes"
    );
}

#[test]
fn fs_readfilesync_from_js_code_routes_through_agent_cell_read_file_and_denied_paths_return_a_js_exception()
 {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/secrets/key.pem", b"secret");

    let error = harness
        .execute_js("fs.readFileSync('/secrets/key.pem')")
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("denied") && error.contains("/secrets/key.pem"),
        "expected JS fs.readFileSync denial to surface as a JS exception, got {error}"
    );
}

#[test]
fn fs_writefilesync_from_js_code_routes_through_agent_cell_write_file_and_denied_paths_return_a_js_exception()
 {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_js("fs.writeFileSync('/workspace/blocked.txt', 'blocked')")
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("denied") && error.contains("/workspace/blocked.txt"),
        "expected JS fs.writeFileSync denial to surface as a JS exception, got {error}"
    );
    assert!(
        !harness.vfs.exists("/workspace/blocked.txt"),
        "denied JS writes must not touch the underlying VFS"
    );
}

#[test]
fn simulacra_fs_readfile_and_writefile_also_route_through_agent_cell_proxy_methods() {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/secrets/key.pem", b"secret");

    let read_error = harness
        .execute_js(
            r#"
            import { readFile } from "simulacra:fs";
            readFile("/secrets/key.pem");
            "#,
        )
        .unwrap_err()
        .to_string();
    let write_error = harness
        .execute_js(
            r#"
            import { writeFile } from "simulacra:fs";
            writeFile("/workspace/blocked.txt", "blocked");
            "#,
        )
        .unwrap_err()
        .to_string();

    assert!(
        read_error.contains("denied"),
        "expected simulacra:fs readFile to route through AgentCell read checks, got {read_error}"
    );
    assert!(
        write_error.contains("denied"),
        "expected simulacra:fs writeFile to route through AgentCell write checks, got {write_error}"
    );
}

#[test]
fn console_log_does_not_route_through_the_proxy_and_writes_directly_to_the_virtual_stdout_buffer() {
    let harness = Harness::new(
        capability(&[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let output = harness
        .execute_js("console.log('hello from js')")
        .expect("console.log should write to the JS stdout buffer");

    assert_eq!(output.stdout, "hello from js\n");
    assert_eq!(
        harness.vfs.read_count(),
        0,
        "console.log must not read the VFS"
    );
    assert_eq!(
        harness.vfs.write_count(),
        0,
        "console.log must not write the VFS"
    );
}

#[test]
fn agent_cell_provides_a_modulefetcher_impl_to_the_js_runtime_it_owns() {
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:modules.invalid"], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_js(
            r#"
            import value from "https://modules.invalid/entry.js";
            value;
            "#,
        )
        .unwrap_err()
        .to_string();

    assert!(
        !error.contains("No module fetcher configured"),
        "expected AgentCell-owned JsRuntime to install a ModuleFetcher, got {error}"
    );
}

#[test]
fn remote_module_import_triggers_modulefetcher_fetch_which_delegates_to_agent_cell_fetch_http() {
    let journal = Arc::new(FakeJournalStorage::default());
    let budget = unlimited_budget();
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:modules.invalid"], false, true),
        Arc::clone(&budget),
        Arc::clone(&journal),
    );
    let stub_url = "https://modules.invalid/entry.js";
    harness
        .cell
        .register_module_stub(stub_url, "export default 42;");
    let before = budget_counter(&budget, "used_turns");

    let output = harness.execute_js(
        r#"
        import value from "https://modules.invalid/entry.js";
        value;
        "#,
    );

    // Verify the module fetch produced a journal entry proving delegation happened
    let entries = journal.entries();
    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::HttpRequest { method, url, status }
                if method == "GET" && url == stub_url && *status == 200
        )),
        "remote module fetch must produce an HttpRequest journal entry proving delegation, got {entries:?}"
    );

    // execute_js increments +1; module fetch does not increment turns (shares the enclosing turn).
    // But the import succeeded, which is the real proof of delegation.
    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after,
        before + 1,
        "execute_js with module fetch should consume one turn total"
    );

    // Additionally verify the module actually resolved
    let output = output.expect("module import via stub should succeed");
    assert_eq!(
        output.result.as_deref(),
        Some("42"),
        "the stub module should have been fetched and evaluated"
    );
}

#[test]
fn remote_module_fetch_with_denied_network_capability_fails_with_a_capability_error_message_surfaced_as_a_js_module_loading_error()
 {
    let harness = Harness::new(
        capability_with_network(&[], &[], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .execute_js(
            r#"
            import denied from "https://denied.invalid/pkg.js";
            denied;
            "#,
        )
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("denied.invalid") && error.contains("capability"),
        "expected remote module capability denial to surface in the JS module-loading error, got {error}"
    );
}

#[test]
fn fetch_http_to_an_allowed_host_returns_the_http_response_with_status_headers_and_body() {
    let server = spawn_http_server(
        200,
        &[("content-type", "text/plain"), ("x-test", "sandbox")],
        b"hello over http",
    );
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let response = harness
        .cell
        .fetch_http(&server.url("/allowed"), "GET", &[], None, None)
        .expect("allowed hosts should return a structured HTTP response");

    assert_eq!(response.status, 200);
    assert!(
        response
            .headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("x-test") && value == "sandbox"),
        "expected response headers to be preserved"
    );
    assert_eq!(response.body, b"hello over http".to_vec());
}

#[test]
fn fetch_http_to_a_denied_host_returns_sandboxerror_capability_denied() {
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"blocked");
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:api.github.com"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .cell
        .fetch_http(&server.url("/blocked"), "GET", &[], None, None)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("capability denied") && error.contains("127.0.0.1"),
        "expected denied host fetch to return CapabilityDenied, got {error}"
    );
}

#[test]
fn fetch_http_network_error_returns_http_with_the_url_and_the_failure_reason() {
    // Use port 1 — a privileged port that is almost never listening,
    // avoiding the TOCTOU race of bind-then-drop-then-connect.
    let url = "http://127.0.0.1:1/offline".to_string();
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .cell
        .fetch_http(&url, "GET", &[], None, None)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains(&url),
        "expected network failure to include the URL, got {error}"
    );
    // The error message varies by platform (e.g. "Connection refused" on Linux,
    // "connection refused" on macOS). Just verify the request failed with an Http error.
    assert!(
        matches!(
            harness
                .cell
                .fetch_http(&url, "GET", &[], None, None)
                .unwrap_err(),
            simulacra_sandbox::SandboxError::Http(_)
        ),
        "expected SandboxError::Http for a network failure"
    );
}

#[test]
fn fetch_http_produces_a_sandbox_http_fetch_span_with_url_method_and_status() {
    let server = spawn_http_server(202, &[("content-type", "text/plain")], b"accepted");
    let harness = Harness::new(
        capability_with_network(&[], &[], &["net:127.0.0.1"], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    let url = server.url("/span");

    let (_result, spans, _events) = capture_operation(|| {
        harness
            .cell
            .fetch_http(&url, "POST", &[], Some(b"payload"), None)
    });

    assert!(
        spans.iter().any(|span| {
            span.fields.get("simulacra.operation.name") == Some(&"sandbox_http_fetch".to_string())
                && span.fields.get("simulacra.http.url") == Some(&url)
                && span.fields.get("simulacra.http.method") == Some(&"POST".to_string())
                && span.fields.get("simulacra.http.status") == Some(&"202".to_string())
        }),
        "expected sandbox_http_fetch span with simulacra.http.url, simulacra.http.method, and simulacra.http.status"
    );
}

// ---------------------------------------------------------------------------
// SB1: read_file journaling coverage
// ---------------------------------------------------------------------------

#[test]
fn read_file_with_valid_capability_writes_a_toolresult_journal_entry() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/hello.txt", b"hello");

    harness
        .read_file("/workspace/hello.txt")
        .expect("read should succeed with valid capability");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ToolResult {
                    tool_name,
                    content,
                    is_error,
                    ..
                } if tool_name == "read_file"
                    && content.contains("5 bytes")
                    && content.contains("/workspace/hello.txt")
                    && !is_error
            )
        }),
        "expected a ToolResult journal entry for read_file with path and byte count, got {entries:?}"
    );
}

// ---------------------------------------------------------------------------
// GSB4: read_file journal entry kind is ToolResult
// ---------------------------------------------------------------------------

#[test]
fn read_file_journal_entry_kind_is_tool_result_not_file_write_or_code_execution() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness
        .vfs
        .seed_file("/workspace/data.bin", b"\x00\x01\x02");

    harness
        .read_file("/workspace/data.bin")
        .expect("read should succeed");

    let entries = journal.entries();
    let read_entries: Vec<_> = entries
        .iter()
        .filter(|e| {
            matches!(
                &e.entry,
                JournalEntryKind::ToolResult { tool_name, .. } if tool_name == "read_file"
            )
        })
        .collect();
    assert_eq!(
        read_entries.len(),
        1,
        "expected exactly one ToolResult journal entry for read_file, got {read_entries:?}"
    );
    // Ensure no FileWrite or CodeExecution entries were written
    assert!(
        !entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::FileWrite { .. } | JournalEntryKind::CodeExecution { .. }
        )),
        "read_file must not produce FileWrite or CodeExecution journal entries"
    );
}

// ---------------------------------------------------------------------------
// SB2/GSB2: read_file budget enforcement
// ---------------------------------------------------------------------------

#[test]
fn read_file_with_budget_exhausted_returns_budget_exhausted_error() {
    let journal = Arc::new(FakeJournalStorage::default());
    // Exhaust the turns budget so check_budget() fails
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        budget_with_overrides(1, 1, 0, 0),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/hello.txt", b"hello");

    let error = harness.read_file("/workspace/hello.txt").unwrap_err();

    assert_budget_exhausted(error, &["turns"], "1", "1");
    // VFS should not have been touched
    harness.vfs.clear_observations();
}

#[test]
fn read_file_with_vfs_bytes_budget_exhausted_returns_budget_exhausted_error() {
    let journal = Arc::new(FakeJournalStorage::default());
    // Exhaust the vfs_bytes budget
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        budget_with_overrides(0, 0, 100, 100),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/hello.txt", b"hello");

    let error = harness.read_file("/workspace/hello.txt").unwrap_err();

    assert_budget_exhausted(error, &["vfs_bytes"], "100", "100");
}

// ---------------------------------------------------------------------------
// SB3: Capability denial does NOT consume budget
// ---------------------------------------------------------------------------

#[test]
fn capability_denial_on_read_file_does_not_increment_used_turns() {
    let budget = budget_with_overrides(10, 0, 0, 0);
    let harness = Harness::new(
        capability(&[], &[], false, false), // no read capability
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/workspace/secret.txt", b"secret");
    let before = budget_counter(&budget, "used_turns");

    let _ = harness.read_file("/workspace/secret.txt");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after, before,
        "capability denial on read_file must not increment used_turns"
    );
}

#[test]
fn capability_denial_on_write_file_does_not_increment_used_turns() {
    let budget = budget_with_overrides(10, 0, 0, 0);
    let harness = Harness::new(
        capability(&[], &[], false, false), // no write capability
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    let _ = harness.write_file("/workspace/denied.txt", b"denied");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after, before,
        "capability denial on write_file must not increment used_turns"
    );
}

#[test]
fn capability_denial_on_execute_shell_does_not_increment_used_turns() {
    let budget = budget_with_overrides(10, 0, 0, 0);
    let harness = Harness::new(
        capability(&[], &[], false, false), // shell = false
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    let _ = harness.execute_shell("echo denied");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after, before,
        "capability denial on execute_shell must not increment used_turns"
    );
}

#[test]
fn capability_denial_on_execute_js_does_not_increment_used_turns() {
    let budget = budget_with_overrides(10, 0, 0, 0);
    let harness = Harness::new(
        capability(&[], &[], false, false), // javascript = false
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    let _ = harness.execute_js("1 + 1");

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after, before,
        "capability denial on execute_js must not increment used_turns"
    );
}

#[test]
fn capability_denial_on_fetch_http_does_not_increment_used_turns() {
    let server = spawn_http_server(200, &[("content-type", "text/plain")], b"ok");
    let budget = budget_with_overrides(10, 0, 0, 0);
    let harness = Harness::new(
        capability_with_network(&[], &[], &[], false, false), // no network capability
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    let before = budget_counter(&budget, "used_turns");

    let _ = harness
        .cell
        .fetch_http(&server.url("/denied"), "GET", &[], None, None);

    let after = budget_counter(&budget, "used_turns");
    assert_eq!(
        after, before,
        "capability denial on fetch_http must not increment used_turns"
    );
}

// ---------------------------------------------------------------------------
// GSB1: list_dir on a denied path returns CapabilityDenied
// ---------------------------------------------------------------------------

#[test]
fn list_dir_on_path_outside_paths_read_returns_capability_denied() {
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/secrets/key.pem", b"secret");

    let error = harness.list_dir("/secrets").unwrap_err();

    assert!(
        matches!(
            error,
            ExpectedSandboxError::CapabilityDenied(CapabilityDenied { ref operation, .. })
                if operation == "read_file"
        ),
        "expected CapabilityDenied for list_dir on denied path, got {error:?}"
    );
}

#[test]
fn list_dir_on_denied_path_does_not_touch_vfs() {
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.seed_file("/secrets/key.pem", b"secret");
    harness.vfs.clear_observations();

    let _ = harness.list_dir("/secrets");

    assert_eq!(
        harness.vfs.list_count(),
        0,
        "denied list_dir must not hit the VFS"
    );
}

// ---------------------------------------------------------------------------
// GSB5: journal write ordering relative to VFS execution for read_file
// ---------------------------------------------------------------------------

#[test]
fn read_file_journal_entry_is_written_after_successful_vfs_read() {
    // Verify that a successful read_file produces a journal entry
    // (the journal entry contains the byte count, which means VFS read
    // must have completed before the journal write).
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness
        .vfs
        .seed_file("/workspace/ordered.txt", b"ordered content");

    let data = harness
        .read_file("/workspace/ordered.txt")
        .expect("read should succeed");

    assert_eq!(data, b"ordered content");
    let entries = journal.entries();
    let tool_result = entries
        .iter()
        .find(|e| {
            matches!(
                &e.entry,
                JournalEntryKind::ToolResult { tool_name, .. } if tool_name == "read_file"
            )
        })
        .expect("expected a ToolResult journal entry after read_file");
    // The journal entry should contain the correct byte count, proving
    // the VFS read completed before the journal write.
    match &tool_result.entry {
        JournalEntryKind::ToolResult { content, .. } => {
            assert!(
                content.contains("15 bytes"),
                "journal entry should reflect the actual bytes read (15), got: {content}"
            );
        }
        _ => unreachable!(),
    }
}

#[test]
fn read_file_on_missing_path_writes_an_error_journal_entry() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    let _ = harness.read_file("/workspace/missing.txt");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::ToolResult { tool_name, is_error, content, .. }
                if tool_name == "read_file" && *is_error && content.contains("missing.txt")
        )),
        "a failed read_file should produce an error journal entry, got {entries:?}"
    );
}

// ---------------------------------------------------------------------------
// GSB12: fs.readFileSync/fs.writeFileSync from JS — success path
// ---------------------------------------------------------------------------

#[test]
fn fs_readfilesync_from_js_on_allowed_path_returns_file_content() {
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness
        .vfs
        .seed_file("/workspace/greeting.txt", b"hello from vfs");

    let output = harness
        .execute_js("fs.readFileSync('/workspace/greeting.txt')")
        .expect("fs.readFileSync on an allowed path should succeed");

    assert_eq!(
        output.result.as_deref(),
        Some("hello from vfs"),
        "fs.readFileSync should return the file content as a string"
    );
}

#[test]
fn fs_readfilesync_from_js_on_allowed_path_produces_journal_entry() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, true),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness
        .vfs
        .seed_file("/workspace/journaled.txt", b"journal me");

    harness
        .execute_js("fs.readFileSync('/workspace/journaled.txt')")
        .expect("fs.readFileSync on an allowed path should succeed");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::ToolResult { tool_name, is_error, .. }
                if tool_name == "read_file" && !is_error
        )),
        "fs.readFileSync from JS should produce a ToolResult journal entry via the FsProxy, got {entries:?}"
    );
}

#[test]
fn fs_writefilesync_from_js_on_allowed_path_writes_to_vfs_and_produces_journal_entry() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &["/output/**"], false, true),
        unlimited_budget(),
        Arc::clone(&journal),
    );

    harness
        .execute_js("fs.writeFileSync('/output/result.txt', 'written from js')")
        .expect("fs.writeFileSync on an allowed path should succeed");

    // Verify VFS state
    assert!(
        harness.vfs.exists("/output/result.txt"),
        "fs.writeFileSync should write to the VFS"
    );
    let data = harness.vfs.inner.read("/output/result.txt").unwrap();
    assert_eq!(
        String::from_utf8_lossy(&data),
        "written from js",
        "fs.writeFileSync should write the correct content"
    );

    // Verify journal entry
    let entries = journal.entries();
    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::FileWrite { path, .. }
                if path == "/output/result.txt"
        )),
        "fs.writeFileSync from JS should produce a FileWrite journal entry via the FsProxy, got {entries:?}"
    );
}

#[test]
fn js_fs_extended_operations_are_journaled_and_spanned_through_fs_proxy() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &["/workspace/**"], false, true),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/old.txt", b"move me");
    harness.vfs.seed_file("/workspace/delete.txt", b"delete me");

    let (result, spans, _) = capture_operation(|| {
        harness.execute_js(
            r#"
            fs.mkdirSync('/workspace/newdir');
            fs.readdirSync('/workspace');
            const stat = fs.statSync('/workspace/old.txt');
            if (!stat.isFile || stat.size !== 7) throw new Error('bad stat');
            fs.renameSync('/workspace/old.txt', '/workspace/newdir/new.txt');
            fs.unlinkSync('/workspace/delete.txt');
            'done';
            "#,
        )
    });

    let output = result.expect("extended JS fs operations should succeed through FsProxy");
    assert_eq!(output.result.as_deref(), Some("done"));

    for expected in [
        "sandbox_fs_proxy_mkdir",
        "sandbox_fs_proxy_list_dir",
        "sandbox_fs_proxy_stat",
        "sandbox_fs_proxy_rename",
        "sandbox_fs_proxy_remove",
    ] {
        assert!(
            spans.iter().any(|span| span
                .fields
                .get("simulacra.operation.name")
                .is_some_and(|operation| operation == expected)),
            "expected span {expected}, got {spans:?}"
        );
    }

    let entries = journal.entries();
    for expected in ["mkdir", "list_dir", "stat", "rename", "remove"] {
        assert!(
            entries.iter().any(|entry| matches!(
                &entry.entry,
                JournalEntryKind::ToolResult { tool_name, is_error, .. }
                    if tool_name == expected && !is_error
            )),
            "expected successful {expected} ToolResult journal entry, got {entries:?}"
        );
    }
}

#[test]
fn js_append_file_sync_requires_only_write_capability() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&[], &["/workspace/**"], false, true),
        unlimited_budget(),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/log.txt", b"first");

    let output = harness
        .execute_js(
            r#"
            fs.appendFileSync('/workspace/log.txt', ' second');
            fs.appendFileSync('/workspace/new.txt', 'created');
            'done';
            "#,
        )
        .expect("appendFileSync should be a mediated write operation");

    assert_eq!(output.result.as_deref(), Some("done"));
    assert_eq!(
        harness.vfs.read("/workspace/log.txt").unwrap(),
        b"first second"
    );
    assert_eq!(harness.vfs.read("/workspace/new.txt").unwrap(), b"created");
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::FileWrite { path, size_bytes }
                if path == "/workspace/log.txt" && *size_bytes == 7
        )),
        "expected appendFileSync to journal appended byte count as FileWrite"
    );
}

#[test]
fn js_rename_sync_moves_directories_with_write_capability_on_both_roots() {
    let harness = Harness::new(
        capability(&[], &["/workspace/from", "/workspace/to"], false, true),
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    );
    harness.vfs.mkdir("/workspace/from").unwrap();
    harness.vfs.seed_file("/workspace/from/a.txt", b"a");
    harness.vfs.mkdir("/workspace/from/sub").unwrap();
    harness.vfs.seed_file("/workspace/from/sub/b.txt", b"b");

    harness
        .execute_js("fs.renameSync('/workspace/from', '/workspace/to');")
        .expect("renameSync should move directories through the mediated host operation");

    assert!(!harness.vfs.exists("/workspace/from"));
    assert_eq!(harness.vfs.read("/workspace/to/a.txt").unwrap(), b"a");
    assert_eq!(harness.vfs.read("/workspace/to/sub/b.txt").unwrap(), b"b");
}
