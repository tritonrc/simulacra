// The sandbox spans a native file tool opens must stay under the span that was
// current when the tool was called, even though the cell call now runs on a
// blocking thread; and a listing entry that vanishes mid-list is still a
// tool error.

// ---------------------------------------------------------------------------
// Span continuity across the blocking thread
// ---------------------------------------------------------------------------

/// A span seen by the process-wide capture layer, with the names of every
/// ancestor (nearest first).
#[derive(Debug, Clone)]
struct SeenSpan {
    name: String,
    ancestors: Vec<String>,
}

fn seen_spans() -> &'static Mutex<Vec<SeenSpan>> {
    static SEEN: OnceLock<Mutex<Vec<SeenSpan>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(Vec::new()))
}

struct AncestryLayer;

impl<S> tracing_subscriber::Layer<S> for AncestryLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let ancestors = ctx
            .span(id)
            .map(|span| span.scope().skip(1).map(|s| s.name().to_string()).collect())
            .unwrap_or_default();
        seen_spans().lock().unwrap().push(SeenSpan {
            name: attrs.metadata().name().to_string(),
            ancestors,
        });
    }
}

/// `tracing::subscriber::with_default` is thread-local and a blocking-pool
/// thread never sees it, so the only subscriber that can observe a span opened
/// on a blocking thread is the global one. Install it once for this binary;
/// every other test in the binary scopes its own subscriber with
/// `with_default`, which takes precedence on its thread. Each test below uses
/// a unique outer span name so concurrent tests cannot see each other's spans.
fn install_global_ancestry_capture() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let subscriber = tracing_subscriber::registry::Registry::default().with(AncestryLayer);
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other test in this binary installs a global subscriber");
        tracing::callsite::rebuild_interest_cache();
    });
}

/// Runs one tool call inside `outer`. The caller must have installed the
/// global capture before creating `outer`: a span made with no subscriber
/// installed is disabled and would have no children to find.
fn call_inside(
    harness: &Harness,
    outer: tracing::Span,
    name: &str,
    arguments: Value,
    capability: &CapabilityToken,
) -> Result<Value, ToolError> {
    let _guard = registry_call_guard();
    tracing::callsite::rebuild_interest_cache();
    // The registry opens its invocation span when `call` is *called*, so the
    // outer span must be entered before that, not only while polling.
    let _entered = outer.enter();
    run_async(harness.registry.call(name, arguments, capability))
}

/// Every span called `span_name` whose ancestry includes `outer_name`.
fn spans_under(outer_name: &str, span_name: &str) -> Vec<SeenSpan> {
    seen_spans()
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.name == span_name && s.ancestors.iter().any(|a| a == outer_name))
        .cloned()
        .collect()
}

/// Positive control: the capture saw the tool's own invocation span under the
/// outer span, so an empty result for the sandbox span means it lost its
/// parent rather than that nothing was captured.
fn assert_capture_is_live(outer_name: &str) {
    let live = seen_spans()
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.ancestors.iter().any(|a| a == outer_name));
    assert!(
        live,
        "the capture layer saw no span under {outer_name}; it is not observing this test"
    );
}

fn assert_sandbox_span_under(outer_name: &str, sandbox_span: &str) {
    assert_capture_is_live(outer_name);
    let found = spans_under(outer_name, sandbox_span);
    if found.is_empty() {
        let all: Vec<SeenSpan> = seen_spans()
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.name == sandbox_span)
            .cloned()
            .collect();
        panic!(
            "{sandbox_span} was not a descendant of {outer_name}; it was opened on the \
             blocking thread without the caller's span. All {sandbox_span} spans seen: {all:?}"
        );
    }
}

#[test]
fn file_read_sandbox_span_stays_under_the_callers_span() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/a.txt", b"hello").unwrap();

    install_global_ancestry_capture();
    let value = call_inside(
        &harness,
        tracing::info_span!("tool_call_outer_read"),
        "file_read",
        json!({"path": "/workspace/a.txt"}),
        &capability,
    )
    .expect("file_read should succeed");

    assert_eq!(tool_content(&value), "hello");
    assert_sandbox_span_under("tool_call_outer_read", "sandbox_read_file");
}

#[test]
fn file_write_sandbox_span_stays_under_the_callers_span() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    install_global_ancestry_capture();
    call_inside(
        &harness,
        tracing::info_span!("tool_call_outer_write"),
        "file_write",
        json!({"path": "/workspace/out.txt", "content": "hello"}),
        &capability,
    )
    .expect("file_write should succeed");

    assert_sandbox_span_under("tool_call_outer_write", "sandbox_write_file");
}

#[test]
fn list_dir_sandbox_spans_stay_under_the_callers_span() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/d/a.txt", b"").unwrap();

    install_global_ancestry_capture();
    let value = call_inside(
        &harness,
        tracing::info_span!("tool_call_outer_list"),
        "list_dir",
        json!({"path": "/workspace/d"}),
        &capability,
    )
    .expect("list_dir should succeed");

    assert_eq!(tool_content(&value), "a.txt");
    assert_sandbox_span_under("tool_call_outer_list", "sandbox_list_dir");
    // The per-entry metadata call is a second cell call in the same closure.
    assert_sandbox_span_under("tool_call_outer_list", "sandbox_metadata");
}

#[test]
fn apply_patch_sandbox_spans_stay_under_the_callers_span() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness
        .vfs
        .write("/workspace/edit.txt", b"one\ntwo\nthree\n")
        .unwrap();

    install_global_ancestry_capture();
    let value = call_inside(
        &harness,
        tracing::info_span!("tool_call_outer_patch"),
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: /workspace/edit.txt\n@@\n one\n-two\n+TWO\n three\n*** End Patch"
        }),
        &capability,
    )
    .expect("update patch should succeed");

    assert_ne!(
        value.get("is_error").and_then(Value::as_bool),
        Some(true),
        "{value:?}"
    );
    // Planning reads through the cell; the commit goes through the mutation
    // path. Both run on the blocking thread.
    assert_sandbox_span_under("tool_call_outer_patch", "sandbox_read_file");
    assert_sandbox_span_under("tool_call_outer_patch", "sandbox_apply_vfs_mutations");
}

// ---------------------------------------------------------------------------
// list_dir: an entry that vanishes between the listing and its metadata call
// ---------------------------------------------------------------------------

/// Lists every entry its inner filesystem has, but answers `metadata` for the
/// named path as if the entry had been removed in the meantime.
struct VanishingEntryFs {
    inner: MemoryFs,
    vanishes: &'static str,
}

impl VirtualFs for VanishingEntryFs {
    fn read(&self, p: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(p)
    }
    fn write(&self, p: &str, d: &[u8]) -> Result<(), VfsError> {
        self.inner.write(p, d)
    }
    fn exists(&self, p: &str) -> bool {
        self.inner.exists(p)
    }
    fn list_dir(&self, p: &str) -> Result<Vec<String>, VfsError> {
        self.inner.list_dir(p)
    }
    fn mkdir(&self, p: &str) -> Result<(), VfsError> {
        self.inner.mkdir(p)
    }
    fn remove(&self, p: &str) -> Result<(), VfsError> {
        self.inner.remove(p)
    }
    fn metadata(&self, p: &str) -> Result<FsMetadata, VfsError> {
        if p == self.vanishes {
            return Err(VfsError::NotFound(p.to_owned()));
        }
        self.inner.metadata(p)
    }
    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }
    fn restore(&self, s: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(s)
    }
}

#[test]
fn list_dir_entry_vanishing_before_its_metadata_call_is_a_tool_error() {
    let fs = Arc::new(VanishingEntryFs {
        inner: MemoryFs::new(),
        vanishes: "/d/gone.txt",
    });
    fs.inner.write("/d/gone.txt", b"").unwrap();
    fs.inner.write("/d/kept.txt", b"").unwrap();
    let capability = full_capability();
    let harness = Harness::over_vfs(fs, capability.clone(), unlimited_budget());

    let result = call_tool(&harness, "list_dir", json!({"path": "/d"}), &capability);

    // Only a failure of the *listing itself* (missing or not a directory) is a
    // tool result; a per-entry metadata failure was always a tool error.
    match result {
        Err(ToolError::ExecutionFailed(message)) => {
            assert!(message.contains("not found"), "{message}");
            assert!(message.contains("/d/gone.txt"), "{message}");
        }
        other => panic!("expected Err(ToolError::ExecutionFailed), got {other:?}"),
    }
}
