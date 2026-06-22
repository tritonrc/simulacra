use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use simulacra_hooks::{HookError, HookModule, HookPipeline, Operation, Phase, Verdict};
use simulacra_runtime::HookedVfsLayer;
use simulacra_types::{
    FsMetadata, TenantId, VfsError, VfsEvent, VfsSnapshot, VfsWatcher, VirtualFs,
};
use simulacra_vfs::{MemoryFs, NotifyingFsLayer};
use tokio::time::timeout;

fn tenant() -> TenantId {
    TenantId::parse("tenant-a").unwrap()
}

/// Recorded invocation: `(hook_name, phase, operation, parsed JSON context)`.
type RecordedCall = (String, Phase, Operation, Value);

struct RecordingHook {
    name: String,
    before: Mutex<VecDeque<Verdict>>,
    calls: Arc<Mutex<Vec<RecordedCall>>>,
}

impl RecordingHook {
    fn new(name: &str, verdicts: Vec<Verdict>, calls: Arc<Mutex<Vec<RecordedCall>>>) -> Self {
        Self {
            name: name.to_string(),
            before: Mutex::new(verdicts.into()),
            calls,
        }
    }
}

impl HookModule for RecordingHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke(
        &self,
        phase: Phase,
        operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError> {
        let parsed: Value = serde_json::from_str(context).unwrap_or(Value::Null);
        self.calls
            .lock()
            .unwrap()
            .push((self.name.clone(), phase, operation, parsed));
        Ok(self
            .before
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(Verdict::continue_unchanged))
    }
}

/// A `VirtualFs` decorator that counts `write` and `remove` invocations on
/// its inner store. Used by deny tests to assert that the inner FS is never
/// reached.
struct RecordingFs {
    inner: Arc<dyn VirtualFs>,
    writes: Arc<AtomicUsize>,
    removes: Arc<AtomicUsize>,
}

impl RecordingFs {
    fn new(inner: Arc<dyn VirtualFs>) -> Self {
        Self {
            inner,
            writes: Arc::new(AtomicUsize::new(0)),
            removes: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn write_count(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }

    fn remove_count(&self) -> usize {
        self.removes.load(Ordering::SeqCst)
    }
}

impl VirtualFs for RecordingFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
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
        self.removes.fetch_add(1, Ordering::SeqCst);
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

    fn subscribe(&self, prefix: &str) -> VfsWatcher {
        self.inner.subscribe(prefix)
    }
}

fn notifying_layer() -> Arc<dyn VirtualFs> {
    Arc::new(NotifyingFsLayer::for_tenant(
        tenant(),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
    )) as Arc<dyn VirtualFs>
}

#[tokio::test]
async fn operation_vfs_write_is_routable_through_the_global_hook_chain() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new("record", vec![], Arc::clone(&calls)));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let layer = HookedVfsLayer::new(tenant(), notifying_layer(), Arc::new(pipeline));

    let _ = layer.write("/workspace/file.txt", b"payload");

    let calls = calls.lock().unwrap();
    let matched = calls.iter().any(|(_, phase, op, ctx)| {
        *phase == Phase::Before
            && *op == Operation::VfsWrite
            && ctx.get("tenant").and_then(Value::as_str) == Some("tenant-a")
            && ctx.get("path").and_then(Value::as_str) == Some("/workspace/file.txt")
            && ctx.get("bytes_len").and_then(Value::as_u64) == Some(7)
    });
    assert!(
        matched,
        "expected a Before VfsWrite invocation with v1 ctx schema, got {:?}",
        *calls
    );
}

#[tokio::test]
async fn hooked_vfs_layer_runs_hooks_for_write_and_remove_before_forwarding() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new("record", vec![], Arc::clone(&calls)));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let layer = HookedVfsLayer::new(
        tenant(),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
        Arc::new(pipeline),
    );

    let _ = layer.write("/workspace/file.txt", b"payload");
    let _ = layer.remove("/workspace/file.txt");

    let calls = calls.lock().unwrap();
    assert!(
        calls.iter().any(|(_, _, op, _)| *op == Operation::VfsWrite),
        "expected at least one VfsWrite invocation, got {:?}",
        *calls
    );
}

#[tokio::test]
async fn deny_verdict_blocks_inner_write_returns_hook_denied_and_emits_no_event() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new(
        "deny",
        vec![Verdict::Deny("blocked by policy".to_string())],
        Arc::clone(&calls),
    ));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    // RecordingFs lets us assert that the inner FS is never called.
    let recording = Arc::new(RecordingFs::new(
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>
    ));
    let recording_for_layer: Arc<dyn VirtualFs> = recording.clone();
    let notifying =
        Arc::new(NotifyingFsLayer::for_tenant(tenant(), recording_for_layer)) as Arc<dyn VirtualFs>;
    let layer = HookedVfsLayer::new(tenant(), Arc::clone(&notifying), Arc::new(pipeline));
    let mut watcher = notifying.subscribe("/");

    let err = layer.write("/var/memory/foo.md", b"payload").unwrap_err();
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(
        matches!(&err, VfsError::HookDenied { reason } if reason == "blocked by policy"),
        "expected VfsError::HookDenied with verbatim reason, got {err:?}"
    );
    assert!(
        received.is_err(),
        "no event should be emitted on deny, got {received:?}"
    );
    assert_eq!(
        recording.write_count(),
        0,
        "inner FS write was called despite hook deny"
    );
    assert_eq!(
        recording.remove_count(),
        0,
        "inner FS remove was called despite hook deny on write"
    );
}

#[tokio::test]
async fn deny_verdict_blocks_inner_remove_and_emits_no_event() {
    // Mirror of the deny-write test for `remove`.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new(
        "deny",
        vec![Verdict::Deny("no removes".to_string())],
        Arc::clone(&calls),
    ));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let recording = Arc::new(RecordingFs::new(
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>
    ));
    let recording_for_layer: Arc<dyn VirtualFs> = recording.clone();
    let notifying =
        Arc::new(NotifyingFsLayer::for_tenant(tenant(), recording_for_layer)) as Arc<dyn VirtualFs>;
    let layer = HookedVfsLayer::new(tenant(), Arc::clone(&notifying), Arc::new(pipeline));
    let mut watcher = notifying.subscribe("/");

    let err = layer.remove("/var/memory/foo.md").unwrap_err();
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(matches!(&err, VfsError::HookDenied { reason } if reason == "no removes"));
    assert!(received.is_err(), "no event on deny, got {received:?}");
    assert_eq!(
        recording.remove_count(),
        0,
        "inner remove called despite deny"
    );
}

#[tokio::test]
async fn mutate_verdict_rewrites_path_and_emits_the_mutated_event() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    // v1 schema is `{tenant, path, bytes_len}`. The hook returns the same
    // tenant + bytes_len with a mutated path; only `path` is honored as a
    // mutation.
    let hook = Arc::new(RecordingHook::new(
        "mutate",
        vec![Verdict::Continue(Some(
            json!({
                "tenant": tenant().as_str(),
                "path": "/b",
                "bytes_len": 7,
            })
            .to_string(),
        ))],
        Arc::clone(&calls),
    ));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let notifying = notifying_layer();
    let layer = HookedVfsLayer::new(tenant(), Arc::clone(&notifying), Arc::new(pipeline));
    let mut watcher = notifying.subscribe("/");

    layer.write("/a", b"payload").unwrap();

    let received = timeout(Duration::from_millis(50), watcher.recv()).await;
    assert!(matches!(
        received,
        Ok(Some(VfsEvent::Written { path, len: 7, .. })) if path == std::path::Path::new("/b")
    ));
}

#[tokio::test]
async fn mutate_verdict_modifying_only_bytes_len_is_silently_ignored() {
    // `bytes_len` is informational. A hook that returns a Continue-modified
    // context with a different `bytes_len` (but the same path) must produce
    // a Written event whose `len` reflects the ACTUAL bytes written, not the
    // hook's bogus value.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new(
        "mutate-bytes-len",
        vec![Verdict::Continue(Some(
            json!({
                "tenant": tenant().as_str(),
                "path": "/file.txt",
                "bytes_len": 999_999,
            })
            .to_string(),
        ))],
        Arc::clone(&calls),
    ));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let notifying = notifying_layer();
    let layer = HookedVfsLayer::new(tenant(), Arc::clone(&notifying), Arc::new(pipeline));
    let mut watcher = notifying.subscribe("/");

    layer.write("/file.txt", b"hello").unwrap();

    let received = timeout(Duration::from_millis(50), watcher.recv()).await;
    let ok = matches!(
        &received,
        Ok(Some(VfsEvent::Written { path, len: 5, .. })) if path == std::path::Path::new("/file.txt")
    );
    assert!(
        ok,
        "expected len=5 (actual bytes), not the hook's bogus 999999; got {received:?}"
    );
}

#[tokio::test]
async fn mutate_verdict_modifying_tenant_produces_hook_contract_violation() {
    // Mutating `tenant` is a security pitfall — the layer must reject it
    // with `VfsError::HookContractViolation` and emit no event.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new(
        "mutate-tenant",
        vec![Verdict::Continue(Some(
            json!({
                "tenant": "tenant-imposter",
                "path": "/file.txt",
                "bytes_len": 5,
            })
            .to_string(),
        ))],
        Arc::clone(&calls),
    ));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let notifying = notifying_layer();
    let layer = HookedVfsLayer::new(tenant(), Arc::clone(&notifying), Arc::new(pipeline));
    let mut watcher = notifying.subscribe("/");

    let err = layer.write("/file.txt", b"hello").unwrap_err();
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(
        matches!(err, VfsError::HookContractViolation),
        "expected HookContractViolation, got {err:?}"
    );
    assert!(
        received.is_err(),
        "no event should be emitted on contract violation, got {received:?}"
    );
}

#[tokio::test]
async fn allow_hooks_match_passthrough_behavior() {
    // When all hooks return `Verdict::Continue` without modifications, the
    // write outcome is identical to having no hooks present.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new("allow", vec![], Arc::clone(&calls)));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let layer = HookedVfsLayer::new(
        tenant(),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
        Arc::new(pipeline),
    );

    layer.write("/workspace/file.txt", b"payload").unwrap();

    assert_eq!(layer.read("/workspace/file.txt").unwrap(), b"payload");
}

#[tokio::test]
async fn every_registered_vfs_write_hook_runs_for_each_write_in_chain_order() {
    // Two hooks registered. After one write:
    // - exactly two recorded calls (one per hook),
    // - {names} == {"a", "b"} — both ran,
    // - chain order: hook "a" before hook "b".
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook_a = Arc::new(RecordingHook::new("a", vec![], Arc::clone(&calls)));
    let hook_b = Arc::new(RecordingHook::new("b", vec![], Arc::clone(&calls)));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook_a);
    pipeline.add(Operation::VfsWrite, hook_b);
    let layer = HookedVfsLayer::new(
        tenant(),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
        Arc::new(pipeline),
    );

    layer.write("/workspace/file.txt", b"payload").unwrap();

    let calls = calls.lock().unwrap();
    let before_calls: Vec<&RecordedCall> = calls
        .iter()
        .filter(|(_, phase, op, _)| *phase == Phase::Before && *op == Operation::VfsWrite)
        .collect();
    assert_eq!(
        before_calls.len(),
        2,
        "expected exactly 2 Before VfsWrite calls (one per hook), got {before_calls:?}"
    );
    let mut names: Vec<&str> = before_calls.iter().map(|(n, _, _, _)| n.as_str()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["a", "b"],
        "expected both hooks to run exactly once each"
    );
    assert_eq!(
        before_calls[0].0, "a",
        "expected chain order: hook 'a' runs before hook 'b'"
    );
    assert_eq!(before_calls[1].0, "b");
}

#[tokio::test]
async fn deny_all_vfs_write_hook_blocks_var_memory_write_and_observes_no_event() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new(
        "deny-all",
        vec![Verdict::Deny("no vfs writes".to_string())],
        Arc::clone(&calls),
    ));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let notifying = notifying_layer();
    let layer = HookedVfsLayer::new(tenant(), Arc::clone(&notifying), Arc::new(pipeline));
    let mut watcher = notifying.subscribe("/var/memory");

    let err = layer.write("/var/memory/foo.md", b"payload").unwrap_err();
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(
        matches!(&err, VfsError::HookDenied { reason } if reason == "no vfs writes"),
        "expected HookDenied(\"no vfs writes\"), got {err:?}"
    );
    assert!(
        received.is_err(),
        "no event should be emitted, got {received:?}"
    );
}

#[tokio::test]
async fn kill_verdict_returns_hook_killed_and_emits_no_event() {
    // `Verdict::Kill` propagates as `VfsError::HookKilled`. No event is
    // published.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new(
        "killer",
        vec![Verdict::Kill("catastrophic policy violation".to_string())],
        Arc::clone(&calls),
    ));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let notifying = notifying_layer();
    let layer = HookedVfsLayer::new(tenant(), Arc::clone(&notifying), Arc::new(pipeline));
    let mut watcher = notifying.subscribe("/");

    let err = layer.write("/var/memory/foo.md", b"payload").unwrap_err();
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(
        matches!(&err, VfsError::HookKilled { reason } if reason == "catastrophic policy violation"),
        "expected HookKilled with verbatim reason, got {err:?}"
    );
    assert!(
        received.is_err(),
        "no event should be emitted on kill, got {received:?}"
    );
}

#[tokio::test]
async fn hook_input_context_uses_v1_schema_with_tenant_path_op_and_bytes_len() {
    // Pin the v1 input ctx schema: `{tenant, path, op, bytes_len}` and nothing
    // else. Compare via parsed JSON, not exact-string equality, so trivial
    // formatting changes don't break the test but a schema drift does.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new("schema", vec![], Arc::clone(&calls)));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let layer = HookedVfsLayer::new(
        tenant(),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
        Arc::new(pipeline),
    );

    let _ = layer.write("/workspace/file.txt", b"abc");

    let calls = calls.lock().unwrap();
    let (_, phase, op, ctx) = calls
        .iter()
        .find(|(_, phase, op, _)| *phase == Phase::Before && *op == Operation::VfsWrite)
        .expect("expected a Before VfsWrite invocation");
    assert_eq!(*phase, Phase::Before);
    assert_eq!(*op, Operation::VfsWrite);

    let object = ctx.as_object().expect("ctx must be a JSON object");
    assert_eq!(
        object.get("tenant").and_then(Value::as_str),
        Some("tenant-a")
    );
    assert_eq!(
        object.get("path").and_then(Value::as_str),
        Some("/workspace/file.txt")
    );
    assert_eq!(object.get("op").and_then(Value::as_str), Some("write"));
    assert_eq!(object.get("bytes_len").and_then(Value::as_u64), Some(3));

    // The v1 schema MUST NOT expose `bytes` to the hook chain.
    assert!(
        !object.contains_key("bytes"),
        "v1 schema must not expose `bytes` to hooks: {ctx:?}"
    );
    // No keys beyond the four documented fields. (Adds defensiveness against
    // accidental schema drift in either direction.)
    let keys: Vec<&str> = object.keys().map(String::as_str).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec!["bytes_len", "op", "path", "tenant"],
        "unexpected keys in v1 hook ctx: {keys:?}"
    );
}

#[tokio::test]
async fn hook_input_context_op_field_is_remove_for_remove_operations() {
    // Mirror of the schema test for `remove`: the `op` field must be "remove"
    // when the layer is invoked through `VirtualFs::remove`. This is the bit
    // that disambiguates remove from a zero-byte write — both have
    // `bytes_len == 0` so a hook needs `op` to tell them apart.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new(
        "schema-remove",
        vec![],
        Arc::clone(&calls),
    ));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let layer = HookedVfsLayer::new(
        tenant(),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
        Arc::new(pipeline),
    );

    let _ = layer.remove("/workspace/file.txt");

    let calls = calls.lock().unwrap();
    let (_, _, _, ctx) = calls
        .iter()
        .find(|(_, phase, op, _)| *phase == Phase::Before && *op == Operation::VfsWrite)
        .expect("expected a Before VfsWrite invocation for remove");
    let object = ctx.as_object().expect("ctx must be a JSON object");
    assert_eq!(
        object.get("op").and_then(Value::as_str),
        Some("remove"),
        "remove invocation should carry op=\"remove\", got ctx={ctx:?}"
    );
    assert_eq!(
        object.get("bytes_len").and_then(Value::as_u64),
        Some(0),
        "remove should advertise bytes_len=0"
    );
}

#[tokio::test]
async fn mutate_verdict_modifying_op_produces_hook_contract_violation() {
    // `op` is immutable per the v1 mutation contract — a hook cannot upgrade
    // a `write` to a `remove` (or vice versa). Attempting to do so produces
    // `VfsError::HookContractViolation` and emits no event.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook::new(
        "mutate-op",
        vec![Verdict::Continue(Some(
            json!({
                "tenant": tenant().as_str(),
                "path": "/file.txt",
                "op": "remove",
                "bytes_len": 5,
            })
            .to_string(),
        ))],
        Arc::clone(&calls),
    ));
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let notifying = notifying_layer();
    let layer = HookedVfsLayer::new(tenant(), Arc::clone(&notifying), Arc::new(pipeline));
    let mut watcher = notifying.subscribe("/");

    // Originating call is `write`; the hook tries to flip op to "remove".
    let err = layer.write("/file.txt", b"hello").unwrap_err();
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(
        matches!(err, VfsError::HookContractViolation),
        "expected HookContractViolation when hook flips op, got {err:?}"
    );
    assert!(
        received.is_err(),
        "no event should be emitted on op-mutation contract violation, got {received:?}"
    );
}

/// A `HookModule` that always returns a hook execution error (Internal).
/// Used to drive the "error" outcome in `simulacra.vfs.hook_outcome`.
struct ErroringHook {
    name: String,
}

impl HookModule for ErroringHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke(
        &self,
        _phase: Phase,
        _operation: Operation,
        _context: &str,
    ) -> Result<Verdict, HookError> {
        Err(HookError::ExecutionError {
            hook: self.name.clone(),
            message: "boom".to_string(),
        })
    }
}

#[tokio::test]
async fn hook_chain_internal_error_fails_closed_with_error_outcome_not_violation() {
    // A non-Killed hook chain error (e.g., serde failure or hook execution
    // panic surfaced as `Err`) must fail closed AND map to the `error`
    // outcome, not `violation`. `violation` is reserved for `Continue`
    // returned with a mutation to an immutable field (`tenant` / `op`).
    let hook = Arc::new(ErroringHook {
        name: "boom".to_string(),
    });
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::VfsWrite, hook);
    let notifying = notifying_layer();
    let layer = HookedVfsLayer::new(tenant(), Arc::clone(&notifying), Arc::new(pipeline));
    let mut watcher = notifying.subscribe("/");

    let err = layer.write("/file.txt", b"hello").unwrap_err();
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    // Fail-closed: a hook error blocks the write. The error string carries
    // "hook error: ..." so an operator can distinguish a synthesized failure
    // from a hook-issued Deny.
    assert!(
        matches!(&err, VfsError::HookDenied { reason } if reason.starts_with("hook error: ")),
        "expected fail-closed HookDenied with 'hook error: ' prefix, got {err:?}"
    );
    assert!(
        received.is_err(),
        "no event should be emitted on hook chain error, got {received:?}"
    );
    // Note: the span attribute `simulacra.vfs.hook_outcome = error` is asserted
    // by the o11y test in s039_vfs_write_o11y.rs (Obsidian path); here we pin
    // the error mapping at the API boundary.
}
