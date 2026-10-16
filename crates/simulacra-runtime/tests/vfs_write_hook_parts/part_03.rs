/// A `MemoryFs` whose bounded listings always report truncation, so a test can
/// tell a layer that forwards `list_dir_bounded` from one that rebuilds it.
struct TruncatingFs(MemoryFs);

impl VirtualFs for TruncatingFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.0.read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.0.write(path, data)
    }
    fn exists(&self, path: &str) -> bool {
        self.0.exists(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.0.list_dir(path)
    }
    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.0.mkdir(path)
    }
    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.0.remove(path)
    }
    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        self.0.metadata(path)
    }
    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.0.snapshot()
    }
    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.0.restore(snapshot)
    }
    fn write_if(
        &self,
        path: &str,
        data: &[u8],
        precondition: simulacra_types::Precondition,
    ) -> Result<(), VfsError> {
        self.0.write_if(path, data, precondition)
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.0.rename(from, to)
    }
    fn apply_batch(&self, mutations: &[simulacra_types::FsMutation]) -> Result<(), VfsError> {
        self.0.apply_batch(mutations)
    }
    fn list_dir_bounded(&self, path: &str) -> Result<simulacra_types::DirListing, VfsError> {
        Ok(simulacra_types::DirListing {
            entries: self.0.list_dir(path)?,
            truncated: true,
        })
    }
}

/// A hooked layer over an atomic-capable `MemoryFs` plus the hook-call log and
/// a handle on the inner store.
fn hooked_over_memory() -> (HookedVfsLayer, Arc<MemoryFs>, Arc<Mutex<Vec<RecordedCall>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new("record", vec![], Arc::clone(&calls)));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let inner = Arc::new(MemoryFs::new());
    let layer = HookedVfsLayer::new(
        tenant(),
        Arc::clone(&inner) as Arc<dyn VirtualFs>,
        Arc::new(pipeline),
    );
    (layer, inner, calls)
}

#[tokio::test]
async fn atomic_methods_answer_unsupported_so_every_write_still_runs_its_hooks() {
    use simulacra_types::{FsMutation, Precondition};

    let (layer, inner, calls) = hooked_over_memory();
    inner.write("/a", b"1").unwrap();
    let batch = [FsMutation::Write {
        path: "/c".into(),
        data: vec![3],
        precondition: Precondition::Unconditional,
    }];

    assert!(matches!(
        layer.rename("/a", "/b"),
        Err(VfsError::Unsupported(_))
    ));
    assert!(matches!(
        layer.write_if("/n", b"2", Precondition::Unconditional),
        Err(VfsError::Unsupported(_))
    ));
    assert!(matches!(
        layer.apply_batch(&batch),
        Err(VfsError::Unsupported(_))
    ));

    // Nothing reached the inner store, and no hook was consulted: forwarding
    // would have skipped the hooks, and the caller's per-write emulation is
    // what runs them.
    assert_eq!(inner.read("/a").unwrap(), b"1");
    for path in ["/b", "/n", "/c"] {
        assert!(!inner.exists(path), "{path} must not exist");
    }
    assert!(
        calls.lock().unwrap().is_empty(),
        "an unsupported atomic call must not consume hook invocations"
    );
}

#[tokio::test]
async fn unsupported_atomic_call_leaves_a_queued_deny_verdict_for_the_next_write() {
    use simulacra_types::Precondition;

    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new(
        "deny-once",
        vec![Verdict::Deny("blocked by policy".to_string())],
        Arc::clone(&calls),
    ));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let inner = Arc::new(MemoryFs::new());
    let layer = HookedVfsLayer::new(
        tenant(),
        Arc::clone(&inner) as Arc<dyn VirtualFs>,
        Arc::new(pipeline),
    );

    assert!(matches!(
        layer.write_if("/a", b"1", Precondition::MustBeAbsent),
        Err(VfsError::Unsupported(_))
    ));
    let err = layer.write("/a", b"1").unwrap_err();

    assert!(
        matches!(&err, VfsError::HookDenied { reason } if reason == "blocked by policy"),
        "the first real write must still meet the hook, got {err:?}"
    );
    assert!(!inner.exists("/a"));
}

#[tokio::test]
async fn unsupported_atomic_calls_publish_no_events() {
    let notifying = notifying_layer();
    let layer = HookedVfsLayer::new(
        tenant(),
        Arc::clone(&notifying),
        Arc::new(HookPipeline::new()),
    );
    notifying.write("/a", b"1").unwrap();
    let mut watcher = notifying.subscribe("/");

    let _ = layer.rename("/a", "/b");
    let _ = layer.apply_batch(&[]);
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(received.is_err(), "no event expected, got {received:?}");
    assert!(notifying.exists("/a") && !notifying.exists("/b"));
}

#[tokio::test]
async fn hooked_layer_forwards_list_dir_bounded_without_running_write_hooks() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new("record", vec![], Arc::clone(&calls)));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let inner = TruncatingFs(MemoryFs::new());
    inner.write("/work/a", b"1").unwrap();
    let layer = HookedVfsLayer::new(tenant(), Arc::new(inner), Arc::new(pipeline));

    let listing = layer.list_dir_bounded("/work").unwrap();

    assert_eq!(listing.entries, vec!["a".to_string()]);
    assert!(
        listing.truncated,
        "the inner bound must survive the hook layer"
    );
    assert!(calls.lock().unwrap().is_empty());
}
