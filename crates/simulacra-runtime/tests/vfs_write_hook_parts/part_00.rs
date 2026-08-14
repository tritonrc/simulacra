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

