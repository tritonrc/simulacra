//! `MemoryStoreFs` — VFS layer that intercepts `/var/memory/**` and `/mnt/**`
//! and routes to a durable [`MemoryStore`](simulacra_memory::MemoryStore).
//!
//! Per S037 §14, this layer is the second line of defense for the memory
//! capability: it enforces `MemoryCapability::search_scopes` and
//! `write_scopes` directly, **not** the generic `paths_read`/`paths_write`
//! from `CapabilityToken`. This means an agent with `paths_read = "/**"`
//! still cannot read memory paths unless `MemoryCapability.search_scopes`
//! grants the prefix.
//!
//! The first line of defense is conditional installation: when
//! `MemoryCapability.enabled == false`, `SimulacraEngine::spawn_task` does NOT
//! wrap the VFS stack with this layer at all. Reads/writes to
//! `/var/memory/**` fall through to the inner VFS and return `NotFound`.
//!
//! **Stack position:** `ProcFs → ServiceFs → MemoryStoreFs → MailboxFs →
//! MemoryFs(inner)`. Memory paths are gated here; `/proc/mailbox/**` is
//! handled below by `MailboxFs` (S036).

use std::sync::{Arc, Mutex};

use simulacra_memory::{MemoryRecvOutcome, MemoryStore, RecentWritesBuffer};
use simulacra_types::{
    DirListing, FsMetadata, FsMutation, MemoryCapability, MemoryPath, Precondition, TenantId,
    VfsError, VfsEvent, VfsSnapshot, VfsWatcher, VirtualFs,
};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

mod events;

use events::forward_memory_event;

use crate::atomic_paths::{self, Touch, normalize_mutations};
use crate::path::normalize;

/// Capacity of the per-layer broadcast channel that fans `MemoryEvent`s out
/// as `VfsEvent`s. Sized to match `simulacra-memory`'s internal event channel so
/// a slow watcher doesn't lag noticeably more than the embedder pipeline.
const VFS_EVENT_CHANNEL_CAPACITY: usize = 1024;

/// A VFS layer that gates and routes memory paths. Pass-through for all
/// non-memory paths.
pub struct MemoryStoreFs<V: VirtualFs> {
    inner: V,
    tenant: TenantId,
    store: Arc<dyn MemoryStore>,
    capability: MemoryCapability,
    /// Per-run recent-writes buffer. When present, every successful write
    /// to `/var/memory/**` is also recorded here so the same-run
    /// `semantic_search` can return it via Guarantee 2 (read-your-writes).
    /// When `None`, the layer still works — writes just fall back to
    /// Guarantee 3 (eventually consistent via the background embedder).
    rrwb: Option<Arc<Mutex<RecentWritesBuffer>>>,
    /// S039: per-layer broadcast that fans out `VfsEvent`s to subscribers
    /// of `subscribe(prefix)`. Each layer instance owns its own broadcast
    /// channel and one drain task that translates `MemoryEvent` →
    /// `VfsEvent`, filtering by the layer's bound tenant.
    vfs_sender: broadcast::Sender<VfsEvent>,
    /// JoinHandle for the drain task spawned lazily on the first
    /// `subscribe(...)` call. `None` until first subscription. Aborted on
    /// `Drop` to keep the drain task lifecycle scoped to the layer.
    drain_handle: Mutex<Option<JoinHandle<()>>>,
}

impl<V: VirtualFs> MemoryStoreFs<V> {
    pub fn new(
        inner: V,
        tenant: TenantId,
        store: Arc<dyn MemoryStore>,
        capability: MemoryCapability,
    ) -> Self {
        let (vfs_sender, _rx) = broadcast::channel(VFS_EVENT_CHANNEL_CAPACITY);
        Self {
            inner,
            tenant,
            store,
            capability,
            rrwb: None,
            vfs_sender,
            drain_handle: Mutex::new(None),
        }
    }

    /// Lazily spawn the drain task that pumps `MemoryStore` events into the
    /// per-layer `VfsEvent` broadcast. Must be called from inside a tokio
    /// runtime — `subscribe` is the documented entry point.
    fn ensure_drain_task(&self) {
        // Poison recovery: if a previous holder panicked while holding this
        // mutex, we recover the inner state via `into_inner()` rather than
        // panicking again. The drain task's lifecycle is best-effort — we
        // do not want to add to a failure cascade.
        let mut guard = match self.drain_handle.lock() {
            Ok(g) => g,
            Err(poison) => {
                warn!(
                    "MemoryStoreFs drain handle mutex was poisoned; \
                     recovering inner state and continuing"
                );
                poison.into_inner()
            }
        };
        if guard.is_some() {
            return;
        }

        // Take a fresh subscription on the underlying store. Other
        // subscribers (notably `BackgroundEmbedder`) are unaffected — the
        // store uses a broadcast channel, so every subscriber sees every
        // event.
        let mut subscription = match self.store.subscribe() {
            Ok(rx) => rx,
            Err(e) => {
                warn!(error = %e, "MemoryStoreFs: failed to subscribe to MemoryStore; \
                    VfsEvent fanout disabled for this layer");
                return;
            }
        };
        let sender = self.vfs_sender.clone();
        let bound_tenant = self.tenant.clone();

        let handle = tokio::spawn(async move {
            loop {
                match subscription.recv().await {
                    MemoryRecvOutcome::Event(event) => {
                        forward_memory_event(&sender, &bound_tenant, event);
                    }
                    MemoryRecvOutcome::Lagged { skipped } => {
                        // The downstream broadcast already accounts for
                        // its own overflow via watcher-side `Skipped`.
                        // Lag on the upstream `MemoryStore` channel just
                        // means a write was dropped before we ever saw
                        // it — log and continue.
                        warn!(
                            skipped,
                            "MemoryStoreFs drain lagged on inner MemoryStore channel"
                        );
                    }
                    MemoryRecvOutcome::Closed => break,
                }
            }
        });

        *guard = Some(handle);
    }

    /// Attach a per-run `RecentWritesBuffer` so writes are recorded for
    /// same-run read-your-writes (Guarantee 2). The buffer must be the same
    /// `Arc` passed to the memory tools for the same agent run.
    pub fn with_rrwb(mut self, rrwb: Arc<Mutex<RecentWritesBuffer>>) -> Self {
        self.rrwb = Some(rrwb);
        self
    }

    /// Returns true if the path is under `/var/memory/**` or `/mnt/**`.
    ///
    /// Delegates to [`MemoryPath::is_memory_path_str`] — the single source of
    /// truth shared with `CapabilityToken::check_path_*`. Both layers must
    /// agree on classification or a security gap opens.
    fn is_memory_path(path: &str) -> bool {
        MemoryPath::is_memory_path_str(path)
    }

    /// Memory paths live in a durable store behind this layer, so no atomic
    /// call can span them and the inner filesystem. A call touching one
    /// answers `Unsupported` before anything is applied: a caller that needs
    /// atomicity must handle that (JS rename and patch application degrade to
    /// their emulation). The plain `write` and `remove` they reach enforce
    /// the capability scopes; this layer must not pre-empt them. Atomic calls
    /// are judged on the resolved path, so `/var/memory/../x` is `/x`. A call
    /// that removes or moves away an ancestor of a memory prefix (`/var`,
    /// `/`) takes the owned subtree with it, so it answers `Unsupported` too.
    fn gate(touches: &[Touch]) -> Result<(), VfsError> {
        let takes_memory = |t: &Touch| {
            t.takes_subtree
                && ["/var/memory", "/mnt"]
                    .iter()
                    .any(|prefix| atomic_paths::is_ancestor(&t.path, prefix))
        };
        match touches
            .iter()
            .find(|t| Self::is_memory_path(&t.path) || takes_memory(t))
        {
            Some(t) => Err(VfsError::Unsupported(format!(
                "atomic operation on memory path {}",
                t.path
            ))),
            None => Ok(()),
        }
    }

    /// The path to judge and route a plain call by. A path that already
    /// names memory is kept as spelled, so `MemoryPath::parse` still rejects
    /// `..` and empty segments inside it rather than have them resolved into
    /// an accepted path. Any other spelling is normalized first, so
    /// `/x/../var/memory/a` is judged, and routed to the store, as
    /// `/var/memory/a` is, and the inner filesystem never sees it.
    fn route(path: &str) -> std::borrow::Cow<'_, str> {
        if Self::is_memory_path(path) {
            std::borrow::Cow::Borrowed(path)
        } else {
            std::borrow::Cow::Owned(normalize(path))
        }
    }

    /// Parse and return a canonical `MemoryPath`, or `VfsError::PermissionDenied`
    /// on any validation failure. This is the authoritative canonicalization
    /// point for memory paths — no upstream caller is trusted to have done it.
    fn parse_memory_path(raw: &str) -> Result<MemoryPath, VfsError> {
        MemoryPath::parse(raw)
            .map_err(|e| VfsError::PermissionDenied(format!("invalid memory path '{raw}': {e}")))
    }
}

impl<V: VirtualFs> VirtualFs for MemoryStoreFs<V> {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let path = Self::route(path);
        let path = path.as_ref();
        if !Self::is_memory_path(path) {
            return self.inner.read(path);
        }
        let mem_path = Self::parse_memory_path(path)?;
        if !self.capability.can_read(&mem_path) {
            debug!(path = %path, "memory read rejected by capability");
            return Err(VfsError::PermissionDenied(format!(
                "read outside memory search_scopes: {path}"
            )));
        }
        let (bytes, _version) = self
            .store
            .get(&self.tenant, &mem_path)
            .map_err(|e| match e {
                simulacra_memory::MemoryError::NotFound(p) => VfsError::NotFound(p),
                other => VfsError::Io(other.to_string()),
            })?;
        Ok(bytes)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let path = Self::route(path);
        let path = path.as_ref();
        if !Self::is_memory_path(path) {
            return self.inner.write(path, data);
        }
        let mem_path = Self::parse_memory_path(path)?;
        // /mnt/** is write-denied for agents regardless of capability —
        // only the admin ingestion API writes there (bypassing this layer).
        if mem_path.is_mnt() {
            return Err(VfsError::PermissionDenied(format!(
                "/mnt is admin-ingested only: {path}"
            )));
        }
        if !self.capability.can_write(&mem_path) {
            debug!(path = %path, "memory write rejected by capability");
            return Err(VfsError::PermissionDenied(format!(
                "write outside memory write_scopes: {path}"
            )));
        }
        let version = self
            .store
            .put(&self.tenant, &mem_path, data)
            .map_err(|e| VfsError::Io(e.to_string()))?;

        // Record in the per-run RRWB so same-run semantic_search sees the
        // write via Guarantee 2 (read-your-writes). If no RRWB is attached
        // (memory tools not registered or construction path didn't plumb
        // one), the write is still durable via the store — the agent just
        // falls back to Guarantee 3 (eventually consistent via the
        // background embedder).
        if let Some(rrwb) = self.rrwb.as_ref()
            && let Ok(mut buf) = rrwb.lock()
        {
            buf.record(mem_path, version, data);
        }
        Ok(())
    }

    fn exists(&self, path: &str) -> bool {
        let path = Self::route(path);
        let path = path.as_ref();
        if !Self::is_memory_path(path) {
            return self.inner.exists(path);
        }
        let Ok(mem_path) = Self::parse_memory_path(path) else {
            return false;
        };
        if !self.capability.can_read(&mem_path) {
            return false;
        }
        self.store.exists(&self.tenant, &mem_path).unwrap_or(false)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        let path = Self::route(path);
        let path = path.as_ref();
        if !Self::is_memory_path(path) {
            return self.inner.list_dir(path);
        }
        let mem_path = Self::parse_memory_path(path)?;
        if !self.capability.can_read(&mem_path) {
            return Err(VfsError::PermissionDenied(format!(
                "list outside memory search_scopes: {path}"
            )));
        }
        let entries = self
            .store
            .list_prefix(&self.tenant, &mem_path)
            .map_err(|e| VfsError::Io(e.to_string()))?;
        // Reduce to immediate children (first segment after the prefix).
        let prefix = mem_path.as_str();
        let prefix_with_slash = if prefix.ends_with('/') {
            prefix.to_string()
        } else {
            format!("{prefix}/")
        };
        let mut children: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for entry in entries {
            let full = entry.path.as_str();
            if let Some(rest) = full.strip_prefix(&prefix_with_slash) {
                let first = rest.split('/').next().unwrap_or("").to_string();
                if !first.is_empty() {
                    children.insert(first);
                }
            }
        }
        Ok(children.into_iter().collect())
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        let path = Self::route(path);
        let path = path.as_ref();
        if !Self::is_memory_path(path) {
            return self.inner.mkdir(path);
        }
        // Directories are implicit in the memory store. mkdir on a valid
        // memory path inside write_scopes is a no-op success; outside, it's
        // a PermissionDenied.
        let mem_path = Self::parse_memory_path(path)?;
        if mem_path.is_mnt() {
            return Err(VfsError::PermissionDenied(format!(
                "/mnt is admin-ingested only: {path}"
            )));
        }
        if !self.capability.can_write(&mem_path) {
            return Err(VfsError::PermissionDenied(format!(
                "mkdir outside memory write_scopes: {path}"
            )));
        }
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        let path = Self::route(path);
        let path = path.as_ref();
        if !Self::is_memory_path(path) {
            return self.inner.remove(path);
        }
        let mem_path = Self::parse_memory_path(path)?;
        if mem_path.is_mnt() {
            return Err(VfsError::PermissionDenied(format!(
                "/mnt is admin-ingested only: {path}"
            )));
        }
        if !self.capability.can_write(&mem_path) {
            return Err(VfsError::PermissionDenied(format!(
                "remove outside memory write_scopes: {path}"
            )));
        }
        self.store
            .delete(&self.tenant, &mem_path)
            .map(|_version| ())
            .map_err(|e| match e {
                simulacra_memory::MemoryError::NotFound(p) => VfsError::NotFound(p),
                other => VfsError::Io(other.to_string()),
            })
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        let path = Self::route(path);
        let path = path.as_ref();
        if !Self::is_memory_path(path) {
            return self.inner.metadata(path);
        }
        let mem_path = Self::parse_memory_path(path)?;
        if !self.capability.can_read(&mem_path) {
            return Err(VfsError::PermissionDenied(format!(
                "metadata outside memory search_scopes: {path}"
            )));
        }
        // Simple strategy: a memory path that has a concrete entry returns
        // file metadata; otherwise, if any entries exist under it as a
        // prefix, it's a directory.
        match self.store.get(&self.tenant, &mem_path) {
            Ok((bytes, _version)) => Ok(FsMetadata::file(bytes.len() as u64)),
            Err(simulacra_memory::MemoryError::NotFound(_)) => {
                let children = self
                    .store
                    .list_prefix(&self.tenant, &mem_path)
                    .map_err(|e| VfsError::Io(e.to_string()))?;
                if children.is_empty() {
                    Err(VfsError::NotFound(path.to_string()))
                } else {
                    Ok(FsMetadata::dir())
                }
            }
            Err(e) => {
                warn!(error = %e, "memory metadata read failed");
                Err(VfsError::Io(e.to_string()))
            }
        }
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        // Memory content lives in the durable store, not in session snapshots.
        // Delegate to the inner VFS for workspace/ephemeral state.
        self.inner.snapshot()
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }

    fn subscribe(&self, prefix: &str) -> VfsWatcher {
        // Lazy-spawn the drain task on the first subscribe call. This keeps
        // construction (`MemoryStoreFs::new`) free of any tokio-runtime
        // requirement while ensuring that any consumer who actually wants
        // events gets them.
        self.ensure_drain_task();
        VfsWatcher::new(self.vfs_sender.subscribe(), prefix)
    }

    fn write_if(
        &self,
        path: &str,
        data: &[u8],
        precondition: Precondition,
    ) -> Result<(), VfsError> {
        Self::gate(&atomic_paths::written(path))?;
        self.inner.write_if(&normalize(path), data, precondition)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        Self::gate(&atomic_paths::renamed(from, to))?;
        self.inner.rename(&normalize(from), &normalize(to))
    }

    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        Self::gate(&atomic_paths::batched(mutations))?;
        self.inner.apply_batch(&normalize_mutations(mutations))
    }

    fn list_dir_bounded(&self, path: &str) -> Result<DirListing, VfsError> {
        let path = Self::route(path);
        let path = path.as_ref();
        if Self::is_memory_path(path) {
            // The listing comes from the memory store, not the inner bound.
            return Ok(DirListing::complete(self.list_dir(path)?));
        }
        self.inner.list_dir_bounded(path)
    }
}

impl<V: VirtualFs> Drop for MemoryStoreFs<V> {
    fn drop(&mut self) {
        // Abort the drain task so it does not outlive the layer.
        if let Ok(mut guard) = self.drain_handle.lock()
            && let Some(handle) = guard.take()
        {
            handle.abort();
        }
    }
}
