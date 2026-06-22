use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rust_decimal::Decimal;
use simulacra_types::{
    ArtifactEntry, ArtifactError, ArtifactStore, CapabilityToken, ResourceBudget, VfsError,
    VirtualFs,
};
use simulacra_vfs::{
    ArtifactWriteSink, HookLister, IntegrationLister, MailboxFs, MemoryFs, ProcFs, ProcState,
    ServiceFs, ToolLister,
};

type ArtifactKey = (String, String, String);

#[derive(Default)]
struct RecordingArtifactStore {
    files: Mutex<BTreeMap<ArtifactKey, Vec<u8>>>,
    #[allow(dead_code, clippy::type_complexity)]
    puts: Mutex<Vec<(String, String, String, Vec<u8>)>>,
    fail_put: AtomicBool,
}

impl RecordingArtifactStore {
    fn seed(&self, tenant: &str, task_id: &str, path: &str, data: &[u8]) {
        self.files.lock().unwrap().insert(
            (tenant.to_string(), task_id.to_string(), path.to_string()),
            data.to_vec(),
        );
    }

    fn stored(&self, tenant: &str, task_id: &str, path: &str) -> Option<Vec<u8>> {
        self.files
            .lock()
            .unwrap()
            .get(&(tenant.to_string(), task_id.to_string(), path.to_string()))
            .cloned()
    }

    fn fail_next_put(&self) {
        self.fail_put.store(true, Ordering::SeqCst);
    }
}

impl ArtifactStore for RecordingArtifactStore {
    fn put(
        &self,
        task_id: &str,
        tenant: &str,
        path: &str,
        data: &[u8],
    ) -> Result<(), ArtifactError> {
        if self.fail_put.swap(false, Ordering::SeqCst) {
            return Err(ArtifactError::InvalidPath(path.to_string()));
        }
        self.puts.lock().unwrap().push((
            tenant.to_string(),
            task_id.to_string(),
            path.to_string(),
            data.to_vec(),
        ));
        self.files.lock().unwrap().insert(
            (tenant.to_string(), task_id.to_string(), path.to_string()),
            data.to_vec(),
        );
        Ok(())
    }

    fn get(&self, tenant: &str, task_id: &str, path: &str) -> Result<Vec<u8>, ArtifactError> {
        self.files
            .lock()
            .unwrap()
            .get(&(tenant.to_string(), task_id.to_string(), path.to_string()))
            .cloned()
            .ok_or_else(|| ArtifactError::NotFound(path.to_string()))
    }

    fn list(&self, tenant: &str, task_id: &str) -> Result<Vec<ArtifactEntry>, ArtifactError> {
        let mut entries = self
            .files
            .lock()
            .unwrap()
            .iter()
            .filter(|((entry_tenant, entry_task, _), _)| {
                entry_tenant == tenant && entry_task == task_id
            })
            .map(|((_, _, path), data)| ArtifactEntry {
                path: path.clone(),
                size: data.len() as u64,
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    fn delete_task(&self, tenant: &str, task_id: &str) -> Result<(), ArtifactError> {
        self.files
            .lock()
            .unwrap()
            .retain(|(entry_tenant, entry_task, _), _| {
                entry_tenant != tenant || entry_task != task_id
            });
        Ok(())
    }
}

struct StaticIntegrationLister;

impl IntegrationLister for StaticIntegrationLister {
    fn integration_names(&self) -> Vec<String> {
        vec!["toy-saas".to_string()]
    }

    fn integration_metadata(&self, name: &str) -> Option<String> {
        (name == "toy-saas").then(|| r#"{"base_url":"http://example.test"}"#.to_string())
    }

    fn integration_readme(&self, name: &str) -> Option<String> {
        (name == "toy-saas").then(|| "# Toy SaaS".to_string())
    }

    fn integration_skill_names(&self, _name: &str) -> Vec<String> {
        vec![]
    }
}

struct EmptyToolLister;

impl ToolLister for EmptyToolLister {
    fn tool_names(&self) -> Vec<String> {
        vec![]
    }

    fn tool_json(&self, _name: &str) -> Option<String> {
        None
    }
}

struct EmptyHookLister;

impl HookLister for EmptyHookLister {
    fn hook_names(&self, _operation: &str) -> Vec<String> {
        vec![]
    }
}

fn mailbox_fs(
    inner: Arc<dyn VirtualFs>,
    store: Arc<dyn ArtifactStore>,
) -> MailboxFs<Arc<dyn VirtualFs>> {
    MailboxFs::new(inner, "task-123".to_string(), "tenant-a".to_string(), store)
}

fn composed_procfs(
    inner: Arc<dyn VirtualFs>,
    store: Arc<dyn ArtifactStore>,
) -> ProcFs<ServiceFs<MailboxFs<Arc<dyn VirtualFs>>>> {
    let mailbox = mailbox_fs(inner, store);
    let with_svc = ServiceFs::new(mailbox, Arc::new(StaticIntegrationLister));
    let state = Arc::new(ProcState {
        agent_id: "task-123".to_string(),
        agent_name: "worker".to_string(),
        model: "ollama:llama3".to_string(),
        parent_id: None,
        budget: Arc::new(Mutex::new(ResourceBudget::new(8_192, 12, Decimal::ZERO, 0))),
        capabilities: CapabilityToken::default(),
        tools: Arc::new(EmptyToolLister),
        session_id: "task-123".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: Arc::new(EmptyHookLister),
        turn: Arc::new(AtomicU64::new(0)),
    });
    ProcFs::new(with_svc, state)
}

fn read_string(vfs: &dyn VirtualFs, path: &str) -> String {
    String::from_utf8(vfs.read(path).unwrap()).unwrap()
}

fn assert_virtual_fs<T: VirtualFs>() {}

#[test]
fn mailbox_fs_implements_the_full_virtual_fs_trait() {
    assert_virtual_fs::<MailboxFs<Arc<dyn VirtualFs>>>();
}

#[test]
fn mailbox_fs_stack_order_procfs_servicefs_mailboxfs_memoryfs_routes_mailbox_writes_to_the_store() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    let fs = composed_procfs(inner.clone(), store.clone());
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/proc/mailbox/reports/q1-summary.md", b"pipeline summary")
        .unwrap();

    assert_eq!(
        store
            .stored("tenant-a", "task-123", "reports/q1-summary.md")
            .unwrap(),
        b"pipeline summary"
    );
    assert_eq!(
        read_string(vfs, "/svc/toy-saas/config.json"),
        r#"{"base_url":"http://example.test"}"#
    );
}

#[test]
fn write_to_proc_mailbox_persists_to_artifact_store_and_delegates_to_inner_vfs() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    let fs = mailbox_fs(inner.clone(), store.clone());
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/proc/mailbox/summary.md", b"artifact body")
        .unwrap();

    assert_eq!(
        store.stored("tenant-a", "task-123", "summary.md").unwrap(),
        b"artifact body"
    );
    assert_eq!(
        inner.read("/proc/mailbox/summary.md").unwrap(),
        b"artifact body"
    );
}

#[test]
fn read_from_proc_mailbox_reads_from_artifact_store_as_the_authoritative_source() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    inner
        .write("/proc/mailbox/summary.md", b"stale inner copy")
        .unwrap();
    let store = Arc::new(RecordingArtifactStore::default());
    store.seed("tenant-a", "task-123", "summary.md", b"durable copy");
    let fs = mailbox_fs(inner, store);
    let vfs: &dyn VirtualFs = &fs;

    assert_eq!(
        vfs.read("/proc/mailbox/summary.md").unwrap(),
        b"durable copy"
    );
}

#[test]
fn list_dir_on_proc_mailbox_returns_entries_from_the_artifact_store() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    store.seed("tenant-a", "task-123", "summary.md", b"top-level");
    store.seed("tenant-a", "task-123", "reports/q1.csv", b"q1");
    store.seed("tenant-a", "task-123", "reports/q2.csv", b"q2");
    let fs = mailbox_fs(inner, store);
    let vfs: &dyn VirtualFs = &fs;

    let mut root = vfs.list_dir("/proc/mailbox").unwrap();
    root.sort();
    assert_eq!(root, vec!["reports", "summary.md"]);

    let mut nested = vfs.list_dir("/proc/mailbox/reports").unwrap();
    nested.sort();
    assert_eq!(nested, vec!["q1.csv", "q2.csv"]);
}

#[test]
fn exists_for_mailbox_paths_checks_the_artifact_store() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    store.seed("tenant-a", "task-123", "summary.md", b"body");
    let fs = mailbox_fs(inner, store);

    assert!(fs.exists("/proc/mailbox/summary.md"));
    assert!(!fs.exists("/proc/mailbox/missing.md"));
}

#[test]
fn mkdir_on_mailbox_paths_is_a_no_op_success() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    let fs = mailbox_fs(inner, store);
    let vfs: &dyn VirtualFs = &fs;

    vfs.mkdir("/proc/mailbox/reports").unwrap();
    assert!(vfs.list_dir("/proc/mailbox").unwrap().is_empty());
}

#[test]
fn remove_on_mailbox_paths_returns_permission_denied() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    let fs = mailbox_fs(inner, store);
    let vfs: &dyn VirtualFs = &fs;

    let err = vfs
        .remove("/proc/mailbox/summary.md")
        .expect_err("mailbox artifacts must be immutable once written");

    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

#[test]
fn non_mailbox_paths_pass_through_to_the_inner_vfs_unchanged() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    let fs = mailbox_fs(inner.clone(), store);
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/workspace/input.csv", b"vendor,amount").unwrap();

    assert_eq!(
        inner.read("/workspace/input.csv").unwrap(),
        b"vendor,amount"
    );
}

#[test]
fn nested_mailbox_paths_work_for_write_read_exists_and_listing() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    let fs = mailbox_fs(inner, store);
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/proc/mailbox/reports/flagged/high-value.csv", b"id,amount")
        .unwrap();

    assert!(vfs.exists("/proc/mailbox/reports/flagged/high-value.csv"));
    assert_eq!(
        vfs.read("/proc/mailbox/reports/flagged/high-value.csv")
            .unwrap(),
        b"id,amount"
    );

    let mut reports = vfs.list_dir("/proc/mailbox/reports").unwrap();
    reports.sort();
    assert_eq!(reports, vec!["flagged"]);
}

#[test]
fn artifacts_survive_vfs_drop_because_mailbox_fs_persists_to_durable_storage() {
    let store = Arc::new(RecordingArtifactStore::default());

    {
        let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        let fs = mailbox_fs(inner, store.clone());
        let vfs: &dyn VirtualFs = &fs;
        vfs.write("/proc/mailbox/summary.md", b"persist me")
            .unwrap();
    }

    assert_eq!(
        store.stored("tenant-a", "task-123", "summary.md").unwrap(),
        b"persist me"
    );
}

// ---------------------------------------------------------------------------
// Artifact-write sink (papercut-9)
// ---------------------------------------------------------------------------

/// Captures (path, tenant, size) tuples passed to the sink.
type SinkCalls = Arc<Mutex<Vec<(String, String, u64)>>>;

fn recording_sink() -> (SinkCalls, ArtifactWriteSink) {
    let calls: SinkCalls = Arc::new(Mutex::new(Vec::new()));
    let calls_for_closure = Arc::clone(&calls);
    let sink: ArtifactWriteSink = Arc::new(move |path: &str, tenant: &str, size: u64| {
        calls_for_closure
            .lock()
            .unwrap()
            .push((path.to_string(), tenant.to_string(), size));
    });
    (calls, sink)
}

#[test]
fn write_to_proc_mailbox_invokes_artifact_sink_with_full_path_tenant_and_size() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    let (calls, sink) = recording_sink();

    let fs = MailboxFs::new(
        inner,
        "task-123".to_string(),
        "tenant-a".to_string(),
        store.clone() as Arc<dyn ArtifactStore>,
    )
    .with_artifact_sink(sink);

    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/proc/mailbox/foo.md", b"hello world").unwrap();

    let recorded = calls.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![(
            "/proc/mailbox/foo.md".to_string(),
            "tenant-a".to_string(),
            "hello world".len() as u64
        )]
    );
}

#[test]
fn write_to_non_mailbox_path_does_not_invoke_artifact_sink() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    let (calls, sink) = recording_sink();

    let fs = MailboxFs::new(
        inner,
        "task-123".to_string(),
        "tenant-a".to_string(),
        store.clone() as Arc<dyn ArtifactStore>,
    )
    .with_artifact_sink(sink);

    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/workspace/x.md", b"not a mailbox write")
        .unwrap();

    assert!(
        calls.lock().unwrap().is_empty(),
        "sink must not fire for non-mailbox paths"
    );
}

#[test]
fn failed_artifact_store_put_does_not_invoke_artifact_sink() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    store.fail_next_put();
    let (calls, sink) = recording_sink();

    let fs = MailboxFs::new(
        inner,
        "task-123".to_string(),
        "tenant-a".to_string(),
        store.clone() as Arc<dyn ArtifactStore>,
    )
    .with_artifact_sink(sink);

    let vfs: &dyn VirtualFs = &fs;
    let err = vfs
        .write("/proc/mailbox/foo.md", b"data")
        .expect_err("write should propagate the store error");
    assert!(matches!(err, VfsError::Io(_)));

    assert!(
        calls.lock().unwrap().is_empty(),
        "sink must not fire when store.put fails"
    );
}

#[test]
fn artifact_sink_panic_does_not_propagate_to_write_caller() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    let panicking_sink: ArtifactWriteSink =
        Arc::new(|_path, _tenant, _size| panic!("sink misbehaved"));

    let fs = MailboxFs::new(
        inner,
        "task-123".to_string(),
        "tenant-a".to_string(),
        store.clone() as Arc<dyn ArtifactStore>,
    )
    .with_artifact_sink(panicking_sink);

    let vfs: &dyn VirtualFs = &fs;
    // Even though the sink panics, the write must succeed because the
    // store.put already persisted the bytes.
    vfs.write("/proc/mailbox/foo.md", b"safe").unwrap();
    assert_eq!(
        store.stored("tenant-a", "task-123", "foo.md").unwrap(),
        b"safe"
    );
}
