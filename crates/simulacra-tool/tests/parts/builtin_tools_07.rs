// Native file tools must run their synchronous VFS calls on a blocking
// thread, and `list_dir` must say when the backend cut a listing short.

use simulacra_types::{DirListing, FsMetadata, FsMutation, Precondition, VfsError, VfsSnapshot};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::ThreadId;
use std::time::Duration;

/// A VFS that records the thread of every call. Its first call to the gated
/// operation blocks until a task on the *same* current-thread runtime
/// signals it. If the tool runs that call on the runtime's only thread, the
/// signalling task can never run and the call fails after the timeout.
struct ProbeFs {
    inner: MemoryFs,
    /// A call whose name starts with this blocks (once) until signalled.
    gate_op: &'static str,
    gate: Mutex<Option<Receiver<()>>>,
    calls: Mutex<Vec<(&'static str, ThreadId)>>,
    /// A backend that cuts listings at this many entries.
    list_limit: Option<usize>,
}

impl ProbeFs {
    fn gated(gate_op: &'static str) -> (Arc<Self>, Sender<()>) {
        let (tx, rx) = channel();
        let fs = Self {
            inner: MemoryFs::new(),
            gate_op,
            gate: Mutex::new(Some(rx)),
            calls: Mutex::new(Vec::new()),
            list_limit: None,
        };
        (Arc::new(fs), tx)
    }

    fn truncating(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryFs::new(),
            gate_op: "",
            gate: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            list_limit: Some(limit),
        })
    }

    fn enter(&self, op: &'static str) -> Result<(), VfsError> {
        self.calls
            .lock()
            .unwrap()
            .push((op, std::thread::current().id()));
        if op.starts_with(self.gate_op) {
            if let Some(rx) = self.gate.lock().unwrap().take() {
                rx.recv_timeout(Duration::from_secs(2))
                    .map_err(|_| VfsError::Io(format!("{op} ran on the async worker thread")))?;
            }
        }
        Ok(())
    }

    fn cut(&self, mut entries: Vec<String>) -> DirListing {
        match self.list_limit {
            Some(limit) if entries.len() > limit => {
                entries.truncate(limit);
                DirListing {
                    entries,
                    truncated: true,
                }
            }
            _ => DirListing::complete(entries),
        }
    }

    /// Every VFS call the tool made, none of which may have run on `runtime`.
    fn assert_no_call_on(&self, runtime: ThreadId, expected_op: &str) {
        let calls = self.calls.lock().unwrap();
        assert!(
            calls.iter().any(|(op, _)| op.starts_with(expected_op)),
            "the tool never reached {expected_op}: {calls:?}"
        );
        let on_runtime: Vec<_> = calls.iter().filter(|(_, t)| *t == runtime).collect();
        assert!(
            on_runtime.is_empty(),
            "VFS calls ran on the async worker thread: {on_runtime:?}"
        );
    }
}

impl VirtualFs for ProbeFs {
    fn read(&self, p: &str) -> Result<Vec<u8>, VfsError> {
        self.enter("read")?;
        self.inner.read(p)
    }
    fn write(&self, p: &str, d: &[u8]) -> Result<(), VfsError> {
        self.enter("write")?;
        self.inner.write(p, d)
    }
    fn exists(&self, p: &str) -> bool {
        self.enter("exists").is_ok() && self.inner.exists(p)
    }
    fn list_dir(&self, p: &str) -> Result<Vec<String>, VfsError> {
        self.enter("list_dir")?;
        Ok(self.cut(self.inner.list_dir(p)?).entries)
    }
    fn mkdir(&self, p: &str) -> Result<(), VfsError> {
        self.enter("mkdir")?;
        self.inner.mkdir(p)
    }
    fn remove(&self, p: &str) -> Result<(), VfsError> {
        self.enter("remove")?;
        self.inner.remove(p)
    }
    fn metadata(&self, p: &str) -> Result<FsMetadata, VfsError> {
        self.enter("metadata")?;
        self.inner.metadata(p)
    }
    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }
    fn restore(&self, s: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(s)
    }
    fn write_if(&self, p: &str, d: &[u8], pre: Precondition) -> Result<(), VfsError> {
        self.enter("write_if")?;
        self.inner.write_if(p, d, pre)
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.enter("rename")?;
        self.inner.rename(from, to)
    }
    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        self.enter("apply_batch")?;
        self.inner.apply_batch(mutations)
    }
    fn list_dir_bounded(&self, p: &str) -> Result<DirListing, VfsError> {
        self.enter("list_dir_bounded")?;
        Ok(self.cut(self.inner.list_dir(p)?))
    }
}

/// Calls a tool on a current-thread runtime where a sibling task sends the
/// signal the gated VFS call is waiting for. Returns the tool result and the
/// id of the runtime's only thread.
fn call_with_signal(
    harness: &Harness,
    signal: Sender<()>,
    name: &str,
    arguments: Value,
    capability: &CapabilityToken,
) -> (Result<Value, ToolError>, ThreadId) {
    let _guard = registry_call_guard();
    let result = run_async(async {
        tokio::spawn(async move { signal.send(()).unwrap() });
        harness.registry.call(name, arguments, capability).await
    });
    (result, std::thread::current().id())
}

fn call_listing(harness: &Harness, path: &str, capability: &CapabilityToken) -> Value {
    call_tool(harness, "list_dir", json!({ "path": path }), capability)
        .expect("list_dir should return a result")
}

// ---------------------------------------------------------------------------
// The synchronous VFS call leaves the async worker
// ---------------------------------------------------------------------------

#[test]
fn file_read_runs_its_vfs_call_off_the_async_worker() {
    let (fs, tx) = ProbeFs::gated("read");
    fs.inner.write("/a.txt", b"hello").unwrap();
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let (result, runtime) = call_with_signal(
        &harness,
        tx,
        "file_read",
        json!({"path": "/a.txt"}),
        &capability,
    );

    let value = result.expect("file_read should succeed");
    assert_eq!(tool_content(&value), "hello");
    fs.assert_no_call_on(runtime, "read");
}

#[test]
fn file_read_of_a_missing_file_also_runs_off_the_async_worker() {
    let (fs, tx) = ProbeFs::gated("read");
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let (result, runtime) = call_with_signal(
        &harness,
        tx,
        "file_read",
        json!({"path": "/nope.txt"}),
        &capability,
    );

    let value = result.expect("a missing file is a tool result, not a ToolError");
    assert_eq!(value.get("is_error").and_then(Value::as_bool), Some(true));
    assert_eq!(tool_content(&value), "not found: /nope.txt");
    fs.assert_no_call_on(runtime, "read");
}

#[test]
fn file_write_runs_its_vfs_call_off_the_async_worker() {
    let (fs, tx) = ProbeFs::gated("write");
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let (result, runtime) = call_with_signal(
        &harness,
        tx,
        "file_write",
        json!({"path": "/w/out.txt", "content": "hello"}),
        &capability,
    );

    let value = result.expect("file_write should succeed");
    assert_eq!(tool_content(&value), "wrote 5 bytes to /w/out.txt");
    assert_eq!(fs.inner.read("/w/out.txt").unwrap(), b"hello");
    fs.assert_no_call_on(runtime, "write");
}

#[test]
fn list_dir_runs_its_listing_call_off_the_async_worker() {
    let (fs, tx) = ProbeFs::gated("list_dir");
    fs.inner.write("/d/a.txt", b"").unwrap();
    fs.inner.write("/d/sub/x", b"").unwrap();
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let (result, runtime) =
        call_with_signal(&harness, tx, "list_dir", json!({"path": "/d"}), &capability);

    let value = result.expect("list_dir should succeed");
    assert_eq!(tool_content(&value), "a.txt\nsub/");
    fs.assert_no_call_on(runtime, "list_dir");
}

#[test]
fn list_dir_runs_its_per_entry_metadata_calls_off_the_async_worker() {
    let (fs, tx) = ProbeFs::gated("metadata");
    fs.inner.write("/d/a.txt", b"").unwrap();
    fs.inner.write("/d/sub/x", b"").unwrap();
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let (result, runtime) =
        call_with_signal(&harness, tx, "list_dir", json!({"path": "/d"}), &capability);

    let value = result.expect("list_dir should succeed");
    assert_eq!(tool_content(&value), "a.txt\nsub/");
    fs.assert_no_call_on(runtime, "metadata");
}

#[test]
fn list_dir_on_a_file_also_runs_off_the_async_worker() {
    let (fs, tx) = ProbeFs::gated("list_dir");
    fs.inner.write("/d/a.txt", b"").unwrap();
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let (result, runtime) = call_with_signal(
        &harness,
        tx,
        "list_dir",
        json!({"path": "/d/a.txt"}),
        &capability,
    );

    let value = result.expect("a file is a tool result, not a ToolError");
    assert_error_result_contains(&value, "not a directory");
    fs.assert_no_call_on(runtime, "list_dir");
}

#[test]
fn apply_patch_commit_runs_off_the_async_worker() {
    // MemoryFs is an atomic backend, so the commit is one `apply_batch`.
    let (fs, tx) = ProbeFs::gated("apply_batch");
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let (result, runtime) = call_with_signal(
        &harness,
        tx,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Add File: /workspace/new.txt\n+hello\n*** End Patch"
        }),
        &capability,
    );

    let value = result.expect("add patch should succeed");
    assert_ne!(
        value.get("is_error").and_then(Value::as_bool),
        Some(true),
        "{value:?}"
    );
    assert_eq!(fs.inner.read("/workspace/new.txt").unwrap(), b"hello\n");
    fs.assert_no_call_on(runtime, "apply_batch");
}

#[test]
fn apply_patch_reads_run_off_the_async_worker() {
    // An update reads the target before it commits; those reads and the
    // existence checks around them are VFS calls too.
    let (fs, tx) = ProbeFs::gated("read");
    fs.inner
        .write("/workspace/edit.txt", b"one\ntwo\nthree\n")
        .unwrap();
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let (result, runtime) = call_with_signal(
        &harness,
        tx,
        "apply_patch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: /workspace/edit.txt\n@@\n one\n-two\n+TWO\n three\n*** End Patch"
        }),
        &capability,
    );

    let value = result.expect("update patch should succeed");
    assert_ne!(
        value.get("is_error").and_then(Value::as_bool),
        Some(true),
        "{value:?}"
    );
    assert_eq!(
        fs.inner.read("/workspace/edit.txt").unwrap(),
        b"one\nTWO\nthree\n"
    );
    fs.assert_no_call_on(runtime, "read");
}

// ---------------------------------------------------------------------------
// list_dir reports a listing the backend cut short
// ---------------------------------------------------------------------------

fn listing_lines(value: &Value) -> Vec<String> {
    tool_content(value).lines().map(str::to_owned).collect()
}

#[test]
fn list_dir_says_when_the_listing_was_cut_short() {
    let fs = ProbeFs::truncating(2);
    for n in ["a", "b", "c"] {
        fs.inner.write(&format!("/d/{n}"), b"").unwrap();
    }
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let lines = listing_lines(&call_listing(&harness, "/d", &capability));

    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(&lines[..2], ["a", "b"], "{lines:?}");
    assert!(lines[2].contains("truncated"), "{lines:?}");
}

#[test]
fn a_truncated_listing_still_marks_directories_with_a_slash() {
    let fs = ProbeFs::truncating(2);
    fs.inner.write("/d/a/x", b"").unwrap();
    fs.inner.write("/d/b", b"").unwrap();
    fs.inner.write("/d/c", b"").unwrap();
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let lines = listing_lines(&call_listing(&harness, "/d", &capability));

    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(&lines[..2], ["a/", "b"], "{lines:?}");
    assert!(lines[2].contains("truncated"), "{lines:?}");
}

#[test]
fn an_untruncated_listing_has_no_truncation_line() {
    let fs = ProbeFs::truncating(10);
    for n in ["a", "b", "c"] {
        fs.inner.write(&format!("/d/{n}"), b"").unwrap();
    }
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let lines = listing_lines(&call_listing(&harness, "/d", &capability));

    assert_eq!(lines, ["a", "b", "c"]);
}

#[test]
fn a_listing_exactly_at_the_backend_bound_is_not_reported_as_truncated() {
    let fs = ProbeFs::truncating(3);
    for n in ["a", "b", "c"] {
        fs.inner.write(&format!("/d/{n}"), b"").unwrap();
    }
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let lines = listing_lines(&call_listing(&harness, "/d", &capability));

    assert_eq!(lines, ["a", "b", "c"]);
}

#[test]
fn an_empty_directory_lists_as_empty_text_without_a_truncation_line() {
    let fs = ProbeFs::truncating(2);
    fs.inner.mkdir("/empty").unwrap();
    let capability = full_capability();
    let harness = Harness::over_vfs(fs.clone(), capability.clone(), unlimited_budget());

    let value = call_listing(&harness, "/empty", &capability);

    assert_eq!(tool_content(&value), "");
}

// ---------------------------------------------------------------------------
// Outputs and errors are otherwise unchanged
// ---------------------------------------------------------------------------

#[test]
fn file_read_of_a_missing_file_keeps_its_not_found_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let value = call_tool(
        &harness,
        "file_read",
        json!({"path": "/workspace/missing.txt"}),
        &capability,
    )
    .expect("a missing file is a tool result, not a ToolError");

    assert_eq!(value.get("is_error").and_then(Value::as_bool), Some(true));
    assert_eq!(tool_content(&value), "not found: /workspace/missing.txt");
}

#[test]
fn list_dir_on_a_file_keeps_its_not_a_directory_error() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/readme.md", b"hello").unwrap();

    let value = call_listing(&harness, "/workspace/readme.md", &capability);

    assert_error_result_contains(&value, "not a directory");
    assert!(
        tool_content(&value).contains("/workspace/readme.md"),
        "{value:?}"
    );
}

#[test]
fn file_write_then_file_read_round_trips_through_the_tools() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let written = call_tool(
        &harness,
        "file_write",
        json!({"path": "/w/note.txt", "content": "héllo"}),
        &capability,
    )
    .expect("file_write should succeed");
    let read = call_tool(
        &harness,
        "file_read",
        json!({"path": "/w/note.txt"}),
        &capability,
    )
    .expect("file_read should succeed");

    assert_eq!(tool_content(&written), "wrote 6 bytes to /w/note.txt");
    assert_eq!(tool_content(&read), "héllo");
}
