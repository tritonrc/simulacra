//! [`NotifyingFsLayer`] — a [`VirtualFs`] decorator that publishes a
//! [`VfsEvent`] for every successful `write`, `remove`, `write_if`, `rename`
//! and `apply_batch` (one event per path written, removed or renamed).
//!
//! Bound to a single tenant at construction; events the layer publishes carry
//! that tenant. Writers never block on slow consumers — the broadcast channel
//! drops oldest events under overflow and the watcher surfaces a
//! [`VfsEvent::Skipped`] sentinel.

use std::sync::{Arc, OnceLock};

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use simulacra_types::{
    DirListing, FsMetadata, FsMutation, Precondition, TenantId, VfsError, VfsEvent, VfsSnapshot,
    VfsWatcher, VirtualFs,
};
use tokio::sync::broadcast;

const DEFAULT_CAPACITY: usize = 256;

// ---------------------------------------------------------------------------
// OTel instruments
// ---------------------------------------------------------------------------

/// `simulacra.vfs.events` — counter incremented on every published `VfsEvent`,
/// keyed by `kind ∈ {written, removed, skipped}` and `layer` (this crate
/// always tags `layer = "notifying"`).
fn vfs_events_counter() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        opentelemetry::global::meter("simulacra-vfs")
            .u64_counter("simulacra.vfs.events")
            .with_description("VfsEvent publications, keyed by kind and layer")
            .build()
    })
}

fn record_event(kind: &'static str) {
    vfs_events_counter().add(
        1,
        &[
            KeyValue::new("kind", kind),
            KeyValue::new("layer", "notifying"),
        ],
    );
}

// ---------------------------------------------------------------------------
// NotifyingFsLayer
// ---------------------------------------------------------------------------

/// A `VirtualFs` decorator that publishes a `VfsEvent` for every successful
/// `write` and `remove`. Bound to a single tenant at construction; the events
/// it publishes carry that tenant.
pub struct NotifyingFsLayer {
    inner: Arc<dyn VirtualFs>,
    tenant: TenantId,
    sender: broadcast::Sender<VfsEvent>,
}

impl NotifyingFsLayer {
    /// Construct a layer bound to `tenant` with the default broadcast capacity
    /// (256). Events published by this layer carry `tenant`.
    pub fn for_tenant(tenant: TenantId, inner: Arc<dyn VirtualFs>) -> Self {
        Self::for_tenant_with_capacity(tenant, inner, DEFAULT_CAPACITY)
    }

    /// Construct a layer bound to `tenant` with an explicit broadcast ring
    /// capacity. Capacity is fixed at construction — there is no chainable
    /// `with_capacity` builder, because swapping the sender after subscribers
    /// have been wired would silently orphan them.
    pub fn for_tenant_with_capacity(
        tenant: TenantId,
        inner: Arc<dyn VirtualFs>,
        capacity: usize,
    ) -> Self {
        let (sender, _receiver) = broadcast::channel(capacity);
        Self {
            inner,
            tenant,
            sender,
        }
    }

    /// Publish a `VfsEvent::Written` if there are live subscribers; bumps the
    /// `simulacra.vfs.events` counter regardless. Send errors (no subscribers)
    /// are intentionally swallowed — the layer publishes best-effort.
    fn publish_written(&self, path: &str, len: u64) {
        record_event("written");
        let _ = self.sender.send(VfsEvent::Written {
            tenant: self.tenant.clone(),
            path: std::path::PathBuf::from(path),
            len,
        });
    }

    /// A rename publishes `Removed(from)` then `Written(to)`, one pair for the
    /// top-level path only: renaming a directory does not publish an event
    /// per descendant. The call does not carry the destination's size, so it
    /// is read back after the rename; that read can see a concurrent writer's
    /// size rather than the renamed entry's, and if it fails (the entry was
    /// moved on or removed meanwhile) the event reports 0. The event still
    /// names the path correctly either way; only `len` is best-effort.
    fn publish_renamed(&self, from: &str, to: &str) {
        self.publish_removed(from);
        let len = self.inner.metadata(to).map_or(0, |m| m.size);
        self.publish_written(to, len);
    }

    fn publish_removed(&self, path: &str) {
        record_event("removed");
        let _ = self.sender.send(VfsEvent::Removed {
            tenant: self.tenant.clone(),
            path: std::path::PathBuf::from(path),
        });
    }
}

impl VirtualFs for NotifyingFsLayer {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.inner.write(path, data)?;
        self.publish_written(path, data.len() as u64);
        Ok(())
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
        self.inner.remove(path)?;
        self.publish_removed(path);
        Ok(())
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
        VfsWatcher::new(self.sender.subscribe(), prefix)
    }

    fn write_if(
        &self,
        path: &str,
        data: &[u8],
        precondition: Precondition,
    ) -> Result<(), VfsError> {
        self.inner.write_if(path, data, precondition)?;
        self.publish_written(path, data.len() as u64);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.inner.rename(from, to)?;
        self.publish_renamed(from, to);
        Ok(())
    }

    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        // Publish only once the whole batch has committed: a failed batch
        // changed nothing, so it must tell no one anything.
        self.inner.apply_batch(mutations)?;
        for mutation in mutations {
            match mutation {
                FsMutation::Write { path, data, .. } => {
                    self.publish_written(path, data.len() as u64);
                }
                FsMutation::Remove { path, .. } => self.publish_removed(path),
                FsMutation::Rename { from, to, .. } => self.publish_renamed(from, to),
                // `mkdir` publishes nothing outside a batch either.
                FsMutation::Mkdir { .. } => {}
            }
        }
        Ok(())
    }

    fn list_dir_bounded(&self, path: &str) -> Result<DirListing, VfsError> {
        self.inner.list_dir_bounded(path)
    }
}
