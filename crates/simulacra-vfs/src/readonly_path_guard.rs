//! S045 — `ReadOnlyPathGuard`: a thin VFS wrapper that delegates reads
//! to an inner filesystem but rejects writes/removes/mkdir for paths
//! under a configured prefix.
//!
//! Used by the engine to enforce that snapshot-backed namespaces are read-only
//! at the composed-VFS level, even though the underlying `MemoryFs` is writable.
//! Writes outside the prefix delegate normally. The atomic calls (`write_if`,
//! `rename`, `apply_batch`) are judged the same way, every touched path at
//! once, so a refused batch applies none of itself.

use std::sync::Arc;

use simulacra_types::{
    DirListing, FsMetadata, FsMutation, Precondition, VfsError, VfsSnapshot, VfsWatcher, VirtualFs,
};

use crate::atomic_paths::{self, Touch, normalize_mutations};
use crate::path::normalize;

pub struct ReadOnlyPathGuard {
    inner: Arc<dyn VirtualFs>,
    /// Prefix that should be treated as read-only. Stored with a trailing
    /// `/` so prefix matching also rejects exact-match (e.g. `mkdir
    /// /var/agent_files`).
    locked_prefix: String,
}

impl ReadOnlyPathGuard {
    pub fn new(inner: Arc<dyn VirtualFs>, locked_prefix: impl Into<String>) -> Self {
        let mut p = locked_prefix.into();
        if !p.ends_with('/') {
            p.push('/');
        }
        Self {
            inner,
            locked_prefix: p,
        }
    }

    fn is_locked(&self, path: &str) -> bool {
        // Lock the prefix itself (without trailing slash) and everything under it.
        let path = normalize(path);
        let trimmed = self.locked_prefix.trim_end_matches('/');
        path == trimmed || path.starts_with(&self.locked_prefix)
    }

    /// True when removing or moving `path` away would take the locked prefix
    /// with it (`/` and every parent of the prefix), though `path` itself is
    /// not locked.
    fn is_ancestor_of_locked(&self, path: &str) -> bool {
        atomic_paths::is_ancestor(path, &self.locked_prefix)
    }

    /// Refuses a call before it reaches `inner` if any touched path is locked.
    /// Checking every path first keeps a refused batch from applying a prefix
    /// of itself.
    fn gate(&self, touches: &[Touch]) -> Result<(), VfsError> {
        match touches.iter().find(|t| {
            self.is_locked(&t.path) || (t.takes_subtree && self.is_ancestor_of_locked(&t.path))
        }) {
            Some(t) => Err(self.rofs(&t.path)),
            None => Ok(()),
        }
    }

    fn rofs(&self, path: &str) -> VfsError {
        VfsError::PermissionDenied(format!(
            "path is read-only ({}): {path}",
            self.locked_prefix.trim_end_matches('/')
        ))
    }
}

impl VirtualFs for ReadOnlyPathGuard {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        if self.is_locked(path) {
            return Err(self.rofs(path));
        }
        self.inner.write(path, data)
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        if self.is_locked(path) {
            return Err(self.rofs(path));
        }
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.gate(&atomic_paths::removed(path))?;
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

    fn write_if(
        &self,
        path: &str,
        data: &[u8],
        precondition: Precondition,
    ) -> Result<(), VfsError> {
        self.gate(&atomic_paths::written(path))?;
        self.inner.write_if(&normalize(path), data, precondition)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.gate(&atomic_paths::renamed(from, to))?;
        self.inner.rename(&normalize(from), &normalize(to))
    }

    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        self.gate(&atomic_paths::batched(mutations))?;
        self.inner.apply_batch(&normalize_mutations(mutations))
    }

    fn list_dir_bounded(&self, path: &str) -> Result<DirListing, VfsError> {
        self.inner.list_dir_bounded(path)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use simulacra_types::{VfsError, VirtualFs};

    use super::ReadOnlyPathGuard;
    use crate::MemoryFs;

    #[test]
    fn write_rejects_normalized_paths_inside_locked_prefix() {
        let fs = Arc::new(MemoryFs::new());
        fs.write("/skills/runbook/SKILL.md", b"original").unwrap();
        let guarded = ReadOnlyPathGuard::new(fs as Arc<dyn VirtualFs>, "/skills");

        let err = guarded
            .write("/workspace/../skills/runbook/SKILL.md", b"tampered")
            .expect_err("normalized path inside locked prefix must be read-only");

        assert!(matches!(err, VfsError::PermissionDenied(_)));
    }
}
