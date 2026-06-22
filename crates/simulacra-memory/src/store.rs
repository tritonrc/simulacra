//! `MemoryStore` trait — durable tenant-scoped key-value storage for memory
//! content. Source of truth for `/var/memory/**` and `/mnt/**`.
//!
//! See S037 §2.

use simulacra_types::{MemoryPath, MemoryVersion, TenantId};
use std::time::SystemTime;

use crate::error::MemoryError;

/// Metadata for a single memory entry. Returned by `list_prefix`.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub path: MemoryPath,
    pub size: u64,
    pub version: MemoryVersion,
    pub mtime: SystemTime,
    pub content_hash: [u8; 32],
}

/// A change event from the store. Subscribed to by the background embedder.
#[derive(Debug, Clone)]
pub enum MemoryEvent {
    Put {
        tenant: TenantId,
        path: MemoryPath,
        version: MemoryVersion,
        content_hash: [u8; 32],
        /// Byte length of the content written. Captured at the publish
        /// site so the S039 `MemoryStoreFs` adapter can surface
        /// `VfsEvent::Written { len }` without re-reading the store
        /// (which would race with subsequent writes/deletes).
        bytes_len: u64,
        produced_at: SystemTime,
    },
    Delete {
        tenant: TenantId,
        path: MemoryPath,
        version: MemoryVersion,
        produced_at: SystemTime,
    },
}

/// Outcome of a `MemoryEventReceiver::recv` call.
#[derive(Debug, Clone)]
pub enum MemoryRecvOutcome {
    /// A fresh event arrived.
    Event(MemoryEvent),
    /// The receiver fell behind and the broadcast ring dropped `skipped`
    /// events. The consumer is expected to recover (e.g., by reading the
    /// persistent store directly to rebuild state).
    Lagged { skipped: u64 },
    /// The store has been dropped and no more events will arrive.
    Closed,
}

/// Subscription handle for memory events. Implementation-defined; the
/// default backend wraps a `tokio::sync::broadcast::Receiver`.
///
/// **Async API:** `recv` is the preferred path for production consumers
/// (e.g., the background embedder). The sync `recv_blocking` method is
/// provided for tests and sync consumers; it MUST NOT be called from
/// inside a tokio runtime (it deadlocks).
pub trait MemoryEventReceiver: Send + Sync {
    /// Await the next event or a lag/closure signal.
    fn recv<'a>(
        &'a mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = MemoryRecvOutcome> + Send + 'a>>;

    /// Block until the next event is available, or return `None` if the
    /// store has been dropped. Lag events are skipped silently — tests that
    /// want explicit lag visibility should use `recv` directly. Do NOT call
    /// from inside a tokio runtime.
    fn recv_blocking(&mut self) -> Option<MemoryEvent>;
}

/// Durable tenant-scoped memory storage.
///
/// Implementations:
/// - **MUST** be linearizable within a tenant (read-after-write returns the
///   new content).
/// - **MUST** atomically write content (concurrent readers see old or new,
///   never partial).
/// - **MUST** bump a monotonic per-path version on every `put` and `delete`.
/// - **MUST** physically isolate tenants (separate database files, no
///   shared global state).
///
/// See S037 §2 for the full contract.
pub trait MemoryStore: Send + Sync + 'static {
    /// Write bytes at `(tenant, path)`. Returns the new monotonic version.
    /// Atomic: readers see either old or new bytes, never partial.
    fn put(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        data: &[u8],
    ) -> Result<MemoryVersion, MemoryError>;

    /// Read bytes and the current version at `(tenant, path)`.
    /// Returns `MemoryError::NotFound` if the path is missing or tombstoned.
    fn get(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
    ) -> Result<(Vec<u8>, MemoryVersion), MemoryError>;

    /// True if the path exists (and is not tombstoned).
    fn exists(&self, tenant: &TenantId, path: &MemoryPath) -> Result<bool, MemoryError>;

    /// List entries under a prefix. Returns metadata only — does not load
    /// content bytes.
    fn list_prefix(
        &self,
        tenant: &TenantId,
        prefix: &MemoryPath,
    ) -> Result<Vec<MemoryEntry>, MemoryError>;

    /// Return the current version for a path, or `None` if missing/tombstoned.
    /// Used by `memory_read_chunk` for the TOCTOU guard.
    fn current_version(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
    ) -> Result<Option<MemoryVersion>, MemoryError>;

    /// Delete a single path. Bumps the tombstone version so stale in-flight
    /// upserts in the index queue become no-ops. Returns the tombstone
    /// version. Returns `MemoryError::NotFound` if the path was never present.
    fn delete(&self, tenant: &TenantId, path: &MemoryPath) -> Result<MemoryVersion, MemoryError>;

    /// Delete everything under a prefix. Returns the number of entries
    /// removed. Used by retention reaper and admin ingestion `replace` mode.
    fn delete_prefix(&self, tenant: &TenantId, prefix: &MemoryPath) -> Result<u64, MemoryError>;

    /// Subscribe to write/delete events.
    fn subscribe(&self) -> Result<Box<dyn MemoryEventReceiver>, MemoryError>;

    /// S038: Ensure the per-tenant backing store is open, migrated, and
    /// ready. Called at CLI bootstrap to convert deferred runtime failures
    /// (corrupt sqlite, missing schema, locked DB) into eager startup
    /// errors. Implementations MUST be idempotent — a second call is a
    /// no-op.
    ///
    /// Default impl returns `Ok(())`; the SQLite implementation overrides
    /// to force a real open + schema migration. See S038 §Bootstrap.
    fn ensure_tenant(&self, _tenant: &TenantId) -> Result<(), MemoryError> {
        Ok(())
    }
}
