//! Mailbox VFS layer (`/proc/mailbox/`).
//!
//! [`MailboxFs`] is a [`VirtualFs`] wrapper that intercepts paths under
//! `/proc/mailbox/` and routes them to a durable [`ArtifactStore`]. All other
//! paths are delegated to the inner VFS unchanged.
//!
//! Writes are dual-written: the artifact store is the authoritative source,
//! while the inner VFS gets a copy for same-session reads by other layers.
//! Reads always come from the artifact store.

use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use simulacra_types::{ArtifactStore, FsMetadata, VfsError, VfsSnapshot, VirtualFs};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// MailboxFs
// ---------------------------------------------------------------------------

/// Sink invoked after a successful mailbox write.
///
/// Arguments are `(full_path, tenant, size_bytes)` — for example
/// `("/proc/mailbox/answer.md", "tenant-a", 42)`. Used by the server to
/// emit `artifact.created` SSE events; kept generic so `simulacra-vfs` does not
/// depend on `simulacra-server`.
///
/// The sink is called *after* `ArtifactStore::put` succeeds. Panics inside
/// the sink are caught and logged — they never propagate to the write
/// caller. A failure to deliver an event is observability noise, not a
/// data-integrity problem.
pub type ArtifactWriteSink = Arc<dyn Fn(&str, &str, u64) + Send + Sync>;

/// A VFS layer that intercepts `/proc/mailbox/**` and routes to an
/// [`ArtifactStore`] for durable artifact persistence.
pub struct MailboxFs<V: VirtualFs> {
    inner: V,
    task_id: String,
    tenant: String,
    store: Arc<dyn ArtifactStore>,
    artifact_sink: Option<ArtifactWriteSink>,
}

impl<V: VirtualFs> MailboxFs<V> {
    pub fn new(inner: V, task_id: String, tenant: String, store: Arc<dyn ArtifactStore>) -> Self {
        Self {
            inner,
            task_id,
            tenant,
            store,
            artifact_sink: None,
        }
    }

    /// Install a sink that is invoked once per successful mailbox write.
    ///
    /// The sink receives the full mailbox path (e.g. `/proc/mailbox/foo.md`),
    /// the tenant namespace, and the byte size of the write. See
    /// [`ArtifactWriteSink`] for failure semantics.
    #[must_use]
    pub fn with_artifact_sink(mut self, sink: ArtifactWriteSink) -> Self {
        self.artifact_sink = Some(sink);
        self
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

const MAILBOX_PREFIX: &str = "/proc/mailbox";

fn is_mailbox(path: &str) -> bool {
    let normalized = path.trim_end_matches('/');
    normalized == MAILBOX_PREFIX || normalized.starts_with("/proc/mailbox/")
}

/// Strip the `/proc/mailbox/` prefix. Returns `None` for `/proc/mailbox` itself.
fn mailbox_tail(path: &str) -> Option<&str> {
    let normalized = path.trim_end_matches('/');
    if normalized == MAILBOX_PREFIX {
        None
    } else {
        normalized.strip_prefix("/proc/mailbox/")
    }
}

// ---------------------------------------------------------------------------
// VirtualFs impl
// ---------------------------------------------------------------------------

impl<V: VirtualFs> VirtualFs for MailboxFs<V> {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        if is_mailbox(path) {
            let tail = mailbox_tail(path).ok_or_else(|| VfsError::NotAFile(path.to_string()))?;

            debug!(
                simulacra.mailboxfs.path = path,
                simulacra.mailboxfs.artifact = tail,
                "mailboxfs read"
            );

            return self
                .store
                .get(&self.tenant, &self.task_id, tail)
                .map_err(|_| VfsError::NotFound(path.to_string()));
        }
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        if is_mailbox(path) {
            let tail = mailbox_tail(path).ok_or_else(|| VfsError::NotAFile(path.to_string()))?;

            debug!(
                simulacra.mailboxfs.path = path,
                simulacra.mailboxfs.artifact = tail,
                simulacra.mailboxfs.size = data.len(),
                "mailboxfs write"
            );

            // Store write is authoritative and must succeed.
            self.store
                .put(&self.task_id, &self.tenant, tail, data)
                .map_err(|e| VfsError::Io(e.to_string()))?;

            // Dual-write to inner for same-session reads by other layers.
            // If parent directory doesn't exist, create it and retry.
            if self.inner.write(path, data).is_err() {
                if let Some(parent) = path.rsplit_once('/').map(|(p, _)| p) {
                    let _ = self.inner.mkdir(parent);
                }
                // Retry; if it still fails, that's fine -- store has the data.
                let _ = self.inner.write(path, data);
            }

            // Notify the artifact sink (if any) AFTER the authoritative
            // store.put has succeeded. Panics inside the sink are caught
            // and logged — they must not propagate to the write caller.
            // This is the seam the server uses to emit `artifact.created`
            // SSE events; failure to deliver is observability noise, not
            // a data-integrity problem.
            if let Some(sink) = &self.artifact_sink {
                let path_owned = path.to_string();
                let tenant_owned = self.tenant.clone();
                let size = data.len() as u64;
                let sink = Arc::clone(sink);
                let result = std::panic::catch_unwind(AssertUnwindSafe(move || {
                    sink(&path_owned, &tenant_owned, size);
                }));
                if let Err(payload) = result {
                    let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                        (*s).to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "<non-string panic payload>".to_string()
                    };
                    warn!(
                        simulacra.mailboxfs.path = path,
                        simulacra.mailboxfs.tenant = self.tenant.as_str(),
                        simulacra.mailboxfs.sink_panic = msg.as_str(),
                        "artifact sink panicked; ignoring (write already persisted)"
                    );
                }
            }

            return Ok(());
        }
        self.inner.write(path, data)
    }

    fn exists(&self, path: &str) -> bool {
        if is_mailbox(path) {
            let normalized = path.trim_end_matches('/');
            if normalized == MAILBOX_PREFIX {
                return true;
            }
            let tail = match mailbox_tail(path) {
                Some(t) => t,
                None => return true,
            };

            // Check exact file match.
            if self.store.get(&self.tenant, &self.task_id, tail).is_ok() {
                return true;
            }

            // Check if it's a virtual directory (any artifact has this prefix).
            let prefix_with_slash = format!("{tail}/");
            if let Ok(entries) = self.store.list(&self.tenant, &self.task_id) {
                return entries
                    .iter()
                    .any(|e| e.path.starts_with(&prefix_with_slash));
            }

            return false;
        }
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        if is_mailbox(path) {
            let prefix = mailbox_tail(path).unwrap_or("");

            let entries = self
                .store
                .list(&self.tenant, &self.task_id)
                .map_err(|e| VfsError::Io(e.to_string()))?;

            let mut result = BTreeSet::new();
            let pfx = if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            };

            for entry in &entries {
                // For root listing, prefix is "". For subdir, prefix is e.g. "reports".
                let relative = if pfx.is_empty() {
                    entry.path.as_str()
                } else {
                    match entry.path.strip_prefix(&pfx) {
                        Some(rest) => rest,
                        None => continue,
                    }
                };

                // Take only the first path component (file or directory name).
                let top = match relative.split_once('/') {
                    Some((dir, _)) => dir,
                    None => relative,
                };

                if !top.is_empty() {
                    result.insert(top.to_string());
                }
            }

            return Ok(result.into_iter().collect());
        }
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        if is_mailbox(path) {
            // Directories are implicit in the artifact store.
            return Ok(());
        }
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        if is_mailbox(path) {
            warn!(
                simulacra.mailboxfs.path = path,
                "remove attempt on immutable mailbox artifact"
            );
            return Err(VfsError::PermissionDenied(format!(
                "{path}: mailbox artifacts are immutable"
            )));
        }
        self.inner.remove(path)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        if is_mailbox(path) {
            let normalized = path.trim_end_matches('/');

            // The mailbox root is always a directory.
            if normalized == MAILBOX_PREFIX {
                return Ok(FsMetadata {
                    is_file: false,
                    is_dir: true,
                    size: 0,
                });
            }

            let tail = mailbox_tail(path).ok_or_else(|| VfsError::NotFound(path.to_string()))?;

            // Check exact file match first.
            if let Ok(data) = self.store.get(&self.tenant, &self.task_id, tail) {
                return Ok(FsMetadata {
                    is_file: true,
                    is_dir: false,
                    size: data.len() as u64,
                });
            }

            // Check if it's a virtual directory.
            let prefix_with_slash = format!("{tail}/");
            if let Ok(entries) = self.store.list(&self.tenant, &self.task_id)
                && entries
                    .iter()
                    .any(|e| e.path.starts_with(&prefix_with_slash))
            {
                return Ok(FsMetadata {
                    is_file: false,
                    is_dir: true,
                    size: 0,
                });
            }

            return Err(VfsError::NotFound(path.to_string()));
        }
        self.inner.metadata(path)
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
}
